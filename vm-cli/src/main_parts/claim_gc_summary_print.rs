#[allow(unused_imports)]
use super::super::*;


// Round-7 cross-cutting Fix 2: install mimalloc as the process-wide allocator
// where its thread teardown is reliable. The Windows FFM/G1 loaded probe
// reproduced corrupted mimalloc free-list metadata in thread TLS teardown;
// switching to mimalloc v2 also failed the loaded G1 GC-progress gate. Keep the
// Windows system allocator until that allocator defect has a proven repair.
// The `mimalloc` feature remains enabled by default on the other supported
// platforms, where this global allocator improves the VM's tiny-object load.
#[cfg(all(feature = "mimalloc", not(windows), not(target_os = "linux")))]
#[global_allocator]
pub(crate) static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
// zr z10/mt: on Linux mimalloc sits behind a thin wrapper that counts big
// blocks and, opt-in (`CRATONVM_MIMALLOC_SPARSE_BLOCK_MMAP`), maps big zeroed
// ones on their own, outside mimalloc's huge-page arena; see the module docs.
//
// Phase 5 WP1: outermost, the boot-image capture wrapper. Outside a capture
// window it is one load and a predictable branch; armed (P0..P1 of a capture
// run) it serves every allocation from the fixed-address arena, so mimalloc is
// not entered; see `image_arena`'s module docs.
#[cfg(all(feature = "mimalloc", target_os = "linux"))]
#[global_allocator]
pub(crate) static GLOBAL: image_arena::ImageArenaAlloc<sparse_block_alloc::SparseBlockAlloc> =
    image_arena::ImageArenaAlloc(sparse_block_alloc::SparseBlockAlloc);
#[cfg(all(not(feature = "mimalloc"), target_os = "linux"))]
#[global_allocator]
pub(crate) static GLOBAL: image_arena::ImageArenaAlloc<std::alloc::System> =
    image_arena::ImageArenaAlloc(std::alloc::System);

/// Claim and emit the process-wide shutdown reports once.
///
/// Two of them today: the JIT method summary (`CRATONVM_DBG_JIT_METHOD_STATS`)
/// and the phase-accounting report (`CRATONVM_PHASE_ACCOUNTING`). They share
/// one claim because they share both shutdown routes: `System.exit` never
/// unwinds Rust frames, so it reaches here from the native pre-exit hook, while
/// a normal Java-main return reaches here from the `main-vm` thread. The atomic
/// makes those paths safe to share and prevents future shutdown convergence
/// from printing either report twice.
///
/// The two reports are gated separately *inside* the claim. `jit.method_stats`
/// used to gate the compare-exchange itself; leaving it there would have made
/// the phase report require an unrelated JIT flag. See
/// `docs/observability/phase-accounting.md` §10.3.
/// `--verbose:gc` was passed. Set as soon as the arguments are parsed, because
/// the flag has to be readable from [`maybe_dump_shutdown_reports`], which runs
/// on both exit arms and is handed no `args`. `CRATONVM_GC_STATS` needs no
/// mirror — it is an environment variable and readable from anywhere.
pub(crate) static GC_STATS_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// A WEAK handle to the running VM, stashed as soon as `Vm::new` returns so
/// that [`maybe_dump_shutdown_reports`] can reach `VmHeap::print_gc_summary`.
///
/// That report carries everything in `G1Collector::print_gc_summary` that needs
/// `&self` -- the heap/IHOP/card-clean/tenuring policy state and the
/// `[GC-SUMMARY]` pause percentiles -- plus the card census, the parallel-evac
/// census and the ZGC block. Every one of them was on the NORMAL-RETURN arm
/// only, and `System.exit` never unwinds Rust frames, so no JUnit workload in
/// the suites had ever emitted a single one of those lines. The
/// process-static G1 guard counters that used to sit beside them moved into
/// `gc_metrics::collector_decision_report`, which this function already
/// emitted on both arms; these could not follow, because they are per
/// collector.
///
/// WEAK on purpose. A strong `Arc` here would keep the VM alive past the end
/// of `run()` and change what its `Drop` does and when -- a real behaviour
/// change for a diagnostic. On the `System.exit` arm the VM is unambiguously
/// alive (the hook runs before the process goes away), which is the arm this
/// exists for; on the normal-return arm `run()` prints the summary itself
/// while it still owns the VM, and [`GC_SUMMARY_PRINTED`] stops the two from
/// both firing.
pub(crate) static VM_FOR_SHUTDOWN: std::sync::OnceLock<std::sync::Weak<cratonvm_vm::vm::SharedVm>> =
    std::sync::OnceLock::new();

/// Claimed by whichever of the two call sites reaches `print_gc_summary`
/// first. The normal-return arm prints from inside `run()` (VM alive, values
/// exact); the `System.exit` arm prints from the shutdown hook through
/// [`VM_FOR_SHUTDOWN`]. Exactly one of them runs per process.
pub(crate) static GC_SUMMARY_PRINTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Claim the right to print the GC summary. `true` for the first caller only.
pub(crate) fn claim_gc_summary_print() -> bool {
    GC_SUMMARY_PRINTED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_ok()
}

/// The exit-time time-to-safepoint line (gc-common w1-a, `GcBarrier::ttsp_stats`),
/// ONE formatter for both exit arms so they cannot drift. Printed even at
/// `pauses=0`: zero pauses is a different fact from "not measured".
/// `max_straggler` (gc-common w3-a / w4-f) is the registry `ThreadId` the
/// slowest pause waited for last — the numbering `CRATONVM_DBG_STW_CENSUS=1`'s
/// `[stw-arrive] tid=` lines use; `none` when that pause's quota was met with
/// no participating arrival (no peer, or the last peers were excused by the
/// compiled-code take-over).
pub(crate) fn ttsp_exit_line(t: &cratonvm_vm::threading::gc_barrier::TtspStats) -> String {
    format!(
        "[GC] ttsp: pauses={} avg_us={} max_us={} last_us={} max_straggler={}",
        t.pauses,
        t.total_ns / t.pauses.max(1) / 1_000,
        t.max_ns / 1_000,
        t.last_ns / 1_000,
        t.max_straggler
            .map_or_else(|| "none".to_string(), |tid| tid.to_string()),
    )
}

/// The `CRATONVM_DBG_ROOTFIXUP=1` exit line for the in-pause blocked-thread
/// fold (`thread_registry::blocked_fold_census`, gc-common w4-a; printed since
/// w5-f): `folds` relocating pauses folded their pointer map into `threads`
/// blocked threads' fixup chains in `nanos`. A SHARE of the preceding
/// `[cratonvm] root fix-up:` line's `total` (the fold runs inside
/// `update_all_roots`), never an addition to it. `None` when no fold ran.
pub(crate) fn blocked_fold_exit_line(folds: u64, threads: u64, nanos: u64) -> Option<String> {
    (folds != 0).then(|| {
        format!(
            "[cratonvm] root fix-up blocked_fold: folds={folds} threads={threads} \
total={ms}ms mean={mean}us",
            ms = nanos / 1_000_000,
            mean = nanos / 1_000 / folds.max(1),
        )
    })
}

/// Are GC statistics wanted? The same three-way test `run()` applies, lifted
/// out so the shutdown hook -- which is handed no `args` -- asks the identical
/// question. `g1_dbg_accessor` is in it because the accessor census rides
/// inside `print_gc_summary`.
pub(crate) fn gc_stats_requested() -> bool {
    GC_STATS_REQUESTED.load(std::sync::atomic::Ordering::Acquire)
        || false
        || cratonvm_types::flags::flags().gc.g1_dbg_accessor}

/// LANE W4-B — the nine frame/peer root-audit report lines, on the surface
/// BOTH exit arms reach.
///
/// `w3c-print-gc-summary-and-the-system-exit-arm.md` §3 closes with the rule
/// this function is the answer to:
///
/// > A report emitted from inside `run()` is emitted on one of the two exit
/// > arms. If it must be seen, it belongs in `maybe_dump_shutdown_reports`,
/// > which is called on both. There is no third place.
///
/// These nine were emitted from `run()`'s teardown block, so a program that
/// leaves through `System.exit` — which `junit.textui.TestRunner` does, and
/// which most real programs do — never saw one of them. That is not a
/// symmetric loss across the nine, because three of them are must-be-ZERO
/// CORRECTNESS signals rather than engagement numbers:
///
///  * `xt_peer_scan: unclassified_peers`, whose own comment says a non-zero
///    count means "that cycle marked from an INCOMPLETE root set";
///  * `root_remap_audit: UNREMAPPED`, shouted in capitals at its definition;
///  * `blocked_peer_stack_remap: dropped` (the capture buffer at its cap — a
///    repair OUTAGE) and `unrouted`.
///
/// A correctness signal a normal program cannot print is not a signal: its
/// silence is indistinguishable from a clean run, which is the reading
/// everyone takes.
///
/// # Why this MOVED rather than being duplicated
///
/// Every input is a process-global static or a free function over
/// process-global statics — not one of them touches the `Vm` handle, a lock,
/// or anything `run()` computed — so there is nothing here that needs the VM
/// alive and nothing that a second call site would have to re-derive. W3-C's
/// own structural note ("there should be ONE function both arms call, not two
/// blocks that drift") is the reason it is not duplicated: two copies of a
/// 150-line census is exactly how the `[GC-SUMMARY]`/`collector_decision_report`
/// split came to disagree in the first place.
///
/// The one behaviour difference on the normal-return arm is WHEN the statics
/// are sampled: at `main()`'s call rather than mid-teardown, which is after
/// the non-daemon wait and the shutdown hooks. That is strictly later, so
/// every count is at least what it was, and it is the same sampling point
/// `collector_decision_report` and `a5_frame_pass` already use — which is
/// the point of having one surface. The lines also now appear after those
/// two rather than before them.
///
/// The gate is unchanged: the caller is inside `if gc_stats_requested()`, the
/// same question `run()` asked, asked through the same function.
pub(crate) fn print_frame_root_audit_reports() {
    // Cross-thread STW peer-scan coverage. A non-zero count means the
    // collector swept while a peer it could not classify was still
    // running JIT code, i.e. that cycle marked from an INCOMPLETE root
    // set. See old-sweep-liveness.md.
    use std::sync::atomic::Ordering as O;
    let peers = cratonvm_vm::jit::xt_root_scan::XT_PEERS_UNCLASSIFIED.load(O::Relaxed);
    let cycles = cratonvm_vm::jit::xt_root_scan::XT_CYCLES_WITH_UNCLASSIFIED.load(O::Relaxed);
    // H2-CID0 (2026-08-05): UNCONDITIONAL. This used to print only when
    // `peers > 0`, which made "every peer answered" indistinguishable
    // from "the take-over never ran" — and a run that FAILED printed
    // nothing, which reads as the reassuring one and was the other.
    // `taken_over` is what separates them.
    let taken = cratonvm_vm::jit::xt_root_scan::XT_THREADS_TAKEN_OVER.load(O::Relaxed);
    let roots = cratonvm_vm::jit::xt_root_scan::XT_ROOTS_FOUND.load(O::Relaxed);
    let hw = cratonvm_vm::jit::xt_root_scan::XT_HELPER_WINDOWS_SCANNED.load(O::Relaxed);
    let resig = cratonvm_vm::jit::xt_root_scan::XT_PEER_RESIGNALS.load(O::Relaxed);
    let saved = cratonvm_vm::jit::xt_root_scan::XT_PEERS_CLASSIFIED_AFTER_RETRY.load(O::Relaxed);
    let hw_pin = cratonvm_vm::jit::xt_root_scan::XT_HELPER_WINDOWS_PINNED.load(O::Relaxed);
    let hw_ref = cratonvm_vm::jit::xt_root_scan::XT_HELPER_WINDOWS_REFUSED.load(O::Relaxed);
    // gc-common w6-g: `pauses_with_frozen / takeover_pauses` is the share of
    // take-over pauses that froze a peer in compiled code, the first figure
    // `docs/feature-designs/real-jdk-mode.md` asks for.
    let tp = cratonvm_vm::jit::xt_root_scan::XT_TAKEOVER_PAUSES.load(O::Relaxed);
    let tf = cratonvm_vm::jit::xt_root_scan::XT_TAKEOVER_PAUSES_WITH_FROZEN.load(O::Relaxed);
    eprintln!(
        "[GC] xt_peer_scan: unclassified_peers={peers} cycles_with_unclassified={cycles} \
         taken_over={taken} xt_roots={roots} helper_windows={hw} hw_pinned={hw_pin} hw_refused={hw_ref} \
         resignals={resig} classified_after_retry={saved} takeover_pauses={tp} \
         pauses_with_frozen={tf} enabled={}",
        cratonvm_vm::jit::xt_root_scan::enabled(),
    );
    // Engagement census for the blocked-peer NATIVE-STACK remap
    // (2026-09-07). `captured` is the denominator: a blocked peer's
    // stack words that a cross-thread scan resolved to heap objects.
    // `written` is the repair firing on wake; `skipped` is the guard
    // declining a word the native call reused since the capture.
    // written=0 means the repair never engaged on this run.
    let (bs_c, bs_a, bs_w, bs_s, bs_d, bs_x, bs_u, bs_b, bs_on) =
        cratonvm_vm::blocked_peer_stack_remap_census();
    // `discarded` is a correct discard (the cycle that captured never
    // relocated); `dropped` is the capture buffer at its cap, which is
    // a repair OUTAGE; `unrouted` is a capture on a RELOCATING cycle
    // that no blocked thread claimed, i.e. a word nothing will rewrite.
    // `below_wake` (gcj j1pn raw-write item 2): a word in a frame the wake
    // pushed after the capture, refused by the wake floor.
    eprintln!(
        "[GC] blocked_peer_stack_remap: captured={bs_c} adopted={bs_a} written={bs_w} skipped={bs_s} \
discarded={bs_d} dropped={bs_x} unrouted={bs_u} below_wake={bs_b} enabled={bs_on}"
    );
    // Extent census for the two storage classes
    // `moving-young-corruption-rootcause.md` nominates and that no
    // stale-word census has ever been able to rank: a frame that gave
    // scalar replacement or LICM hoisting no slots has no such region,
    // so a zero word-count against one of those names is a statement
    // about the optimisation rather than about the region.
    // `[jit-vacated-frame]` census (`CRATONVM_DBG_VACATED_FRAMES`).
    // `verifiable` is the number that matters: a slot the coverage
    // machinery claims to describe, still naming an address the last
    // collection vacated.
    {
        let (jv_n, jv_v, jv_per) = cratonvm_vm::jit::conservative_roots::jit_vacated_frame_census();
        if jv_n > 0 {
            let mut per = String::new();
            for (i, name) in cratonvm_vm::jit::conservative_roots::JIT_VACATED_REGION_NAMES
                .iter()
                .enumerate()
            {
                if jv_per[i] > 0 {
                    per.push_str(&format!(" {name}={}", jv_per[i]));
                }
            }
            eprintln!("[GC] jit_vacated_frames: total={jv_n} verifiable={jv_v} by_region:{per}");
        }
    }
    let (fl_n, fl_sc, fl_rh, fl_ar) = cratonvm_jit::region_extent_census();
    eprintln!(
        "[GC] jit_frame_region_extents: frames={fl_n} with_scalar_span={fl_sc} \
with_ref_hoist_span={fl_rh} with_arith_span={fl_ar}"
    );
    // Engagement census for the blocked-peer SHADOW-STACK scan. A
    // clean run with `sh_windows=0` means the scan never ran, and any
    // conclusion drawn from it is vacuous.
    let sh_w = cratonvm_vm::jit::xt_root_scan::XT_PEER_SHADOW_WINDOWS.load(O::Relaxed);
    let sh_s = cratonvm_vm::jit::xt_root_scan::XT_PEER_SHADOW_SLOTS.load(O::Relaxed);
    let sh_r = cratonvm_vm::jit::xt_root_scan::XT_PEER_SHADOW_ROOTS.load(O::Relaxed);
    let sh_u = cratonvm_vm::jit::xt_root_scan::XT_PEER_SHADOW_UNTRUSTED.load(O::Relaxed);
    eprintln!(
        "[GC] xt_peer_shadow: sh_windows={sh_w} sh_slots={sh_s} sh_roots={sh_r} sh_untrusted={sh_u} enabled={}",
        cratonvm_vm::jit::conservative_roots::xt_peer_shadow_scan_enabled(),
    );
    // Take-over twin (gc-common w1-c, printed since w2-e): shadow windows of
    // peers frozen IN compiled code. `to_windows=0` on a run with
    // `taken_over>0` (the `[GC] xt_peer_scan:` line) means the scan never
    // engaged — kill switch, or no peer ever published a window.
    // `stack_incomplete` counts windows whose stack range could not be read
    // (both OS arms) and, since w6-g, take-over windows with indirect entries
    // and no published peer stack band (`bandless`; always the case off
    // Windows/Linux). `rejected_indirect` must be ZERO: non-zero is the JIT
    // publishing a primitive in a reference home (`ShadowOddLongProbe`),
    // contained by the band check.
    let to_w = cratonvm_vm::jit::xt_root_scan::XT_TAKEOVER_SHADOW_WINDOWS.load(O::Relaxed);
    let to_r = cratonvm_vm::jit::xt_root_scan::XT_TAKEOVER_SHADOW_ROOTS.load(O::Relaxed);
    let to_u = cratonvm_vm::jit::xt_root_scan::XT_TAKEOVER_SHADOW_UNPUBLISHED.load(O::Relaxed);
    let to_i = cratonvm_vm::jit::xt_root_scan::XT_TAKEOVER_STACK_INCOMPLETE.load(O::Relaxed);
    let to_b = cratonvm_vm::jit::xt_root_scan::XT_TAKEOVER_SHADOW_BANDLESS.load(O::Relaxed);
    let rej = cratonvm_vm::jit::xt_root_scan::shadow_rejected_indirect_entries();
    eprintln!(
        "[GC] xt_takeover_shadow: to_windows={to_w} to_roots={to_r} to_unpublished={to_u} \
         stack_incomplete={to_i} bandless={to_b} rejected_indirect={rej}"
    );
    // Stale-frame-word audit: words a moving collection left pointing
    // at a pre-move address, i.e. references no oop map named. FRAMES
    // is the denominator -- stale=0 with frames=0 means the audit never
    // ran, not that the frames were clean.
    // Root-remap audit (`CRATONVM_DBG_ROOT_REMAP_AUDIT=1`): scanned
    // roots left naming an address the same collection vacated. The
    // audit only speaks when it finds something, so the three
    // denominators are what make its zero a reading — `moved=0` in
    // particular means every audited cycle was non-moving and the run
    // says nothing at all.
    let (rra_c, rra_r, rra_m, rra_u) = cratonvm_vm::memory::gc::root_remap_audit_stats();
    eprintln!(
        "[GC] root_remap_audit: cycles={rra_c} roots_rescanned={rra_r} \
         moved_entries={rra_m} UNREMAPPED={rra_u}"
    );
    let sfw_f = cratonvm_vm::jit::conservative_roots::stale_frame_audit::FRAMES.load(O::Relaxed);
    let sfw_s = cratonvm_vm::jit::conservative_roots::stale_frame_audit::STALE.load(O::Relaxed);
    let sfw_u =
        cratonvm_vm::jit::conservative_roots::stale_frame_audit::STALE_UNMODELLED.load(O::Relaxed);
    eprintln!(
        "[GC] stale_frame_words: frames_audited={sfw_f} stale={sfw_s} stale_in_unmodelled_slot_class={sfw_u} enabled={}",
        cratonvm_vm::jit::conservative_roots::stale_frame_word_check_enabled(),
    );
    // Unnamed-frame-ref pins: objects a compiled frame named in a slot
    // its oop maps do not. FRAMES is the denominator.
    let urp_f = cratonvm_vm::jit::conservative_roots::unnamed_ref_pins::FRAMES.load(O::Relaxed);
    let urp_p = cratonvm_vm::jit::conservative_roots::unnamed_ref_pins::PINNED.load(O::Relaxed);
    let urp_d = cratonvm_vm::jit::conservative_roots::unnamed_ref_pins::DUPLICATE_OF_MAPPED
        .load(O::Relaxed);
    eprintln!(
        "[GC] unnamed_frame_ref_pins: frames={urp_f} pinned={urp_p} duplicate_of_mapped={urp_d} enabled={}",
        cratonvm_vm::jit::conservative_roots::pin_unnamed_frame_refs_enabled(),
    );
    // Unmapped-duplicate remap: words no oop map named that still held
    // a moved address after the precise remap. FRAMES is the denominator.
    let udr_f = cratonvm_vm::jit::conservative_roots::unmapped_dupe_remap::FRAMES.load(O::Relaxed);
    let udr_r =
        cratonvm_vm::jit::conservative_roots::unmapped_dupe_remap::REWRITTEN.load(O::Relaxed);
    eprintln!(
        "[GC] unmapped_dupe_remap: frames={udr_f} rewritten={udr_r} enabled={}",
        cratonvm_vm::jit::conservative_roots::remap_unmapped_dupes_enabled(),
    );
    // H2-CID0 (2026-08-05): the unregistered-JIT-frame memo's audit.
    // `suppressed` counts times the memo answered "clean" while a scan
    // of the same range found a frame — i.e. oops that went unmarked
    // and a cycle that was never told to avoid moving them.
    // `shortcircuits` is the denominator: without it a zero cannot be
    // told apart from an audit that never ran.
    let sc = cratonvm_vm::jit::conservative_roots::UNREG_MEMO_SHORTCIRCUITS.load(O::Relaxed);
    let sup = cratonvm_vm::jit::conservative_roots::UNREG_MEMO_SUPPRESSED.load(O::Relaxed);
    let supa =
        cratonvm_vm::jit::conservative_roots::UNREG_MEMO_SUPPRESSED_AUTHORITATIVE.load(O::Relaxed);
    // `SUPPRESSED_AUTHORITATIVE` is the one that judges the fix: total
    // suppressions are dominated by ordinary per-native-call snapshots,
    // which no collector marks from.
    eprintln!(
        "[GC] unreg_memo: shortcircuits={sc} SUPPRESSED={sup} \
         SUPPRESSED_AUTHORITATIVE={supa}"
    );
}

pub(crate) fn maybe_dump_shutdown_reports() {
    let vm = cratonvm_vm::native::jni::process_vm_strict();
    maybe_dump_shutdown_reports_for(vm.as_deref());
}

pub(crate) fn maybe_dump_shutdown_reports_for(owning_vm: Option<&cratonvm_vm::SharedVm>) {
    static DUMPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    if DUMPED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }

    // zs s6/rs: the resident set by mapping, FIRST, before any report below
    // allocates (`CRATONVM_DBG_RSS_BY_MAPPING`; a no-op unless set).
    rss_report::print_exit_report(owning_vm);

    // -----------------------------------------------------------------
    // CONTAINED HELPER PANICS — printed first, and never gated.
    // -----------------------------------------------------------------
    //
    // `vm/src/jit/helper_guard.rs` catches a panic crossing an `extern "C"`
    // helper boundary instead of letting it abort the JVM, which is the right
    // default: one bad receiver should not be a total loss. But containment is
    // NOT a recovery. The helper returns its documented failure sentinel and
    // compiled code carries on with it — `jit_instanceof` answers `0` ("not an
    // instance"), `jit_bastore` drops the store, `jit_aastore` drops the store
    // and stashes an `InternalError` delivered at some later drain. A run that
    // produced wrong results FOR THAT REASON scored exactly the same as a
    // clean one on stdout, on a checksum, and on the exit code.
    //
    // The guard does print one stderr line per DISTINCT helper at the moment
    // of its first panic, which is right for a log and useless as a verdict:
    // it is thousands of lines back by the time a run ends, and a repeat is
    // not printed at all. Until this block the counter behind it had exactly
    // ONE consumer in the tree — the crash handler, reached only when
    // something else has already gone fatally wrong.
    //
    // Ungated on purpose, and above the `gc_stats_requested()` block on
    // purpose: this is not an engagement number that a reader opted into, it
    // is a correctness signal, and a reader who has to know the switch's name
    // in order to be told their answers may be wrong will not be told.
    // `jit_helper_panic_summary` returns `None` when the count is zero, so a
    // clean run still prints nothing.
    //
    // nio21 wave 9 lane L4: under `CRATONVM_DBG=exit-census` now. HotSpot's
    // stderr holds no such line, and the guard's own first-panic line per
    // helper is unchanged.
    if exit_census_on() {
        let summary = match owning_vm {
            Some(vm) => cratonvm_vm::jit::helper_guard::jit_helper_panic_summary_for(vm),
            None => cratonvm_vm::jit::helper_guard::jit_helper_panic_summary(),
        };
        if let Some(line) = summary {
            // r23 w1 L11: one exit-report policy (`cratonvm_types::exit_report`).
            cratonvm_types::exit_report::emit(format_args!("{line}"));
        }
    }

    // gce e1/j: the JNI phase census, only when `CRATONVM_DBG_JNI_PHASE=1`
    // armed it (`None` otherwise, and nothing is printed).
    if let Some(line) = cratonvm_vm::native::jni::jni_phase_census_line() {
        eprintln!("{line}");
    }

    // JIT round 21 lane reflect4: the reflective-fallback censuses, each only
    // when its own switch armed it (`CRATONVM_DBG_MH_LINK_CENSUS`,
    // REFLECT4-21-3; `CRATONVM_DBG_BYTECODE_ONLY_BODY`, REFLECT4-21-1).
    for lines in [
        cratonvm_vm::jit::helpers::mh_link_census_lines(),
        cratonvm_vm::jit::helpers::bytecode_only_census_lines(),
    ]
    .into_iter()
    .flatten()
    {
        for line in lines {
            eprintln!("{line}");
        }
    }

    // FIRED TLAB TRIPWIRES — same argument, same placement (gen r4w2/alloc2,
    // 2026-09-23). `FILLER_OVER_OBJECT` / `REFILL_OVER_OBJECT` count a live
    // object buried under a TLAB filler or handed out again by a refill: a
    // use-after-free in waiting. Their `[GC] tlab-guard:` line used to be
    // printed only by `VmHeap::print_gc_summary`, which runs only when GC stats
    // were requested, so a run that tripped them on every collection finished
    // looking clean
    // (`docs/feature-designs/generative-gc-architecture.md`,
    // residue (a)). Printed here whenever a tripwire FIRED, on every backend,
    // EXCEPT when the GC summary will print the same line (with its urgent
    // suffix) itself — whenever GC stats were requested — since printing it
    // twice would make one event look like two. (gen r4w4/alloc4: every
    // backend's `print_gc_summary` prints the census now, so the old
    // "Generational only, and a torn-down VM counts as Generational" test is
    // gone with residue (b).) A clean run prints nothing.
    let summary_prints_guard = gc_stats_requested();
    // nio21 wave 9 lane L4: and under `CRATONVM_DBG=exit-census`.
    if !summary_prints_guard && exit_census_on() && cratonvm_vm::tlab_guard_line_is_urgent() {
        let [_, guard] = cratonvm_vm::tlab_census_lines();
        cratonvm_types::exit_report::emit(format_args!(
            "{guard} — TRIPWIRE FIRED: a live object was buried by a TLAB filler or \
             handed out again by a refill (details on tracing target \
             `cratonvm::gc::guard`; see \
             docs/feature-designs/generative-gc-architecture.md)"
        ));
    }

    // -----------------------------------------------------------------
    // THE UNQUEUED-CODE-FREE AUDIT (RT-2), under CI's own switch.
    // -----------------------------------------------------------------
    //
    // `published_code_free_audit()` returns `(freed, unqueued)`. `unqueued` is
    // called a must-be-zero invariant by three documents
    // (`vm/src/jit/code_cache_lifecycle.rs`, `docs/jit/code-cache-lifecycle.md`,
    // `docs/jit/code-cache-lifetime.md`) and, until `jit/tests/code_free_audit.rs`,
    // its only consumer was a unit test asserting the counter CAN increment —
    // the opposite direction. A bug was already found this way.
    //
    // A whole-VM check is the part a per-crate test cannot do: it sees every
    // compilation a real program produced, through every retirement path, and
    // it is the arrangement in which the deferral arm — an artefact outliving
    // its cache entry, the only window in which an unqueued drop is possible —
    // actually occurs.
    //
    // Behind a switch rather than always on, and PANICKING rather than
    // printing, because those are the two halves of the same decision: a
    // production JVM that has already run the user's program to completion
    // must not abort over a diagnostic, and a CI lane that is asking the
    // question wants a non-zero exit rather than a line in a log nobody
    // greps. What the invariant protects is not small — a thread executing
    // inside a body unmapped outside the retirement queue executes unmapped
    // memory.
    if false {
        let (freed, unqueued) = cratonvm_jit::published_code_free_audit();
        assert_eq!(
            unqueued, 0,
            "{unqueued} published compiled bodies lost their last owner outside the \
             retirement queue ({freed} published bodies unmapped in total). Each was \
             rescued into the queue by `ExecutableBuffer::drop` rather than unmapped \
             under a thread that may be executing it, but each still names a holder \
             that should have released through the queue. \
             `CRATONVM_DBG_JIT_CODE_FREE=1` prints a `[jit-unqueued-free]` line with the \
             dropping owner's backtrace for each one — that is where to look."
        );
    }

    // gcf f2/r: the `gc,exit` heap summary on the `System.exit` arm (the
    // normal-return arm prints it from `run()`, after the hooks, and the
    // logger's own claim makes this a no-op there). Not gated on GC stats:
    // `-Xlog:gc+exit` / `gc*` selects it.
    if let Some(vm) = VM_FOR_SHUTDOWN.get().and_then(std::sync::Weak::upgrade) {
        // zt t9/tg2: HotSpot's `before_exit` stops the ZGC director here.
        cratonvm_vm::runtime::interpreter::stop_zgc_director_tick(&vm);
        // zt t31/x10: and G1's `G1 Service` periodic task.
        cratonvm_vm::runtime::interpreter::stop_g1_periodic_gc_service(&vm);
        // gci i14/hd + zr z9/hp: the collector's own lines (Serial, G1, ZGC).
        cratonvm_vm::runtime::unified_logging::log_gc_exit_summary(|| {
            cratonvm_vm::runtime::unified_logging::gc_exit_heap_lines(&vm)
        });
        // gcj j1/ob: then (Generational) the `safepoint,stats` exit summary.
        cratonvm_vm::runtime::unified_logging::log_safepoint_stats_requested_exit_summary(
            vm.mem.heap.is_generational(),
            true,
        );
        // gcj j1/st item 2: then `-XX:+PrintStringTableStatistics`' block.
        cratonvm_vm::print_string_table_statistics_at_exit(&vm);
        // gcg g1/p: compiled-`new` helper entries on the `System.exit` arm,
        // under `CRATONVM_DBG_JIT_METHOD_STATS` alone (JIT r21 wave 2, lane
        // alloc; once per VM, shared with the normal-return arm's copy).
        if let Some(line) =
            cratonvm_vm::jit::alloc_class_cache::take_compiled_new_paths_line_for(&vm)
        {
            eprintln!("{line}");
        }
    }

    // The collector's own account of WHAT it did and WHY — the decision
    // histogram, the per-reason moving-young fallback rows, and the two G1
    // JIT-root lines (`g1 root coverage`, `g1 jit publication`).
    //
    // `gc_metrics::collector_decision_report` had no production caller at all
    // until 2026-09-01 — `grep` returned its own unit tests — which quietly
    // voided a claim.
    // `bug-g1-evacuates-live-jit-reference-20260819-FIXED.md` keeps
    // `G1Collector::empty_jit_publication` as a detector rather than a fix, on
    // the grounds that with the conservative scan always running under G1 an
    // empty publication under a live compiled frame is once again a genuine
    // anomaly, and says of the line that carries it: "the counter is ungated
    // and should now read zero". Ungated it was. Printed it was not.
    //
    // Emitted from HERE rather than beside `print_gc_summary` in the teardown,
    // because that block is only on the normal-return arm and the workloads
    // this number is wanted for end in `System.exit` — `junit.textui.TestRunner`
    // does, which is the page's own repro. That is the same "detector wired to
    // the arm that does not run" shape as W7-90's slot-map sweep, and this
    // function is where W7-90 put its answer.
    if gc_stats_requested() {
        eprintln!("{}", cratonvm_vm::collector_decision_report());
        // Step 14a5's engagement (`roots::a5_frame_pass`): how many cycles took
        // the non-moving sweep on the A5 unregistered-JIT-frame term alone —
        // where step 1's conservative frame probe, keyed on `is_active()`, was
        // off — and how many roots the repair added there. `cycles=0` says the
        // repair decided nothing in this run, which a passing test cannot
        // otherwise be distinguished from "the repair works". Printed HERE for
        // the same reason as the decision report above: `maybe_dump_shutdown_
        // reports` is on the normal-return arm, and the workloads this number
        // is wanted for end in `System.exit`.
        let (a5_cycles, a5_roots) = cratonvm_vm::memory::roots::a5_frame_pass::census();
        eprintln!("[GC] a5_frame_pass: cycles={a5_cycles} roots={a5_roots}");
        // The above-chain conservative band, same reasoning. It is OPT-IN, so
        // `enabled=false passes=0` is the ordinary reading and says the band
        // was never walked; `enabled=true passes=0` would mean the lever is on
        // but no collection had a live JIT chain, which is a different fact and
        // the one a clean result must not be read against. `bytes` is what the
        // lever costs in stack reads when it is on.
        let (ac_passes, ac_roots, ac_bytes) =
            cratonvm_vm::jit::conservative_roots::above_chain::census();
        eprintln!(
            "[GC] above_chain_scan: passes={ac_passes} roots={ac_roots} bytes={ac_bytes} \
             enabled={}",
            None::<std::ffi::OsString>.is_some(),
        );
        // The collector-state half of the same census -- see
        // `VM_FOR_SHUTDOWN`. Only reachable while the VM is alive, which is
        // the `System.exit` arm; on the normal-return arm `run()` has already
        // claimed the print and this is a no-op.
        // Upgrade BEFORE claiming: a claim that cannot print is a claim that
        // stops anyone else from printing, and the handle is deliberately
        // weak.
        if let Some(vm) = VM_FOR_SHUTDOWN.get().and_then(std::sync::Weak::upgrade) {
            // The `System.exit` arm's copy of the ZGC metrics emission. Its own
            // once-claim, not `claim_gc_summary_print`'s: the TSV row is not
            // gated on GC stats, so it must not be skipped because a summary
            // print was already claimed.
            vm.mem.heap.emit_zgc_end_of_run_metrics();
            if claim_gc_summary_print() {
                // gen r5w2/obs6: publish the VM's allocation total so the
                // summary's `[GC] cards/alloc:` line is current.
                cratonvm_vm::record_vm_allocation_totals(
                    vm.mem
                        .bytes_allocated_total
                        .load(std::sync::atomic::Ordering::Relaxed),
                );
                vm.mem.heap.print_gc_summary();
                // zr z7/rs: what the pre-`main` mimalloc hook did.
                eprintln!("{}", mimalloc_tuning::summary_line());
                // zr z10/mt: big zeroed blocks outside the arena (Linux).
                eprintln!("{}", sparse_block_alloc::summary_line());
                // zs s1/fp: the opt-in first-touch arena (Linux).
                // gcf f4/o: the allocation-failure doors, every collector.
                eprintln!(
                    "{}",
                    cratonvm_vm::runtime::interpreter::oome_ladder_doors_line(&vm)
                );
                // zr z3/lt: ZGC boxing-helper OOME rung (ZGC only).
                if let Some(line) = cratonvm_vm::runtime::interpreter::zgc_jit_box_line(&vm) {
                    eprintln!("{line}");
                }
                // zr z3/ja: stamped / dropped-stale collection requests, every collector.
                if let Some(line) =
                    cratonvm_vm::runtime::interpreter::zgc_stale_gc_requests_line(&vm)
                {
                    eprintln!("{line}");
                }
                // zr z5/cr: ZGC allocation-refusal collections and the futile rule.
                if let Some(line) = cratonvm_vm::runtime::interpreter::zgc_refusal_line(&vm) {
                    eprintln!("{line}");
                }
                // zt t8/rv4: the ZGC metaspace back-fill retry passes.
                if let Some(line) =
                    cratonvm_vm::runtime::interpreter::zgc_metaspace_backfill_line(&vm)
                {
                    eprintln!("{line}");
                }
                // zt t20/x16: the ZGC GC-notification census (D10).
                if let Some(line) =
                    cratonvm_vm::runtime::interpreter::zgc_gc_notifications_line(&vm)
                {
                    eprintln!("{line}");
                }
                // The retained class-layout ledger, every collector.
                eprintln!(
                    "{}",
                    cratonvm_vm::runtime::interpreter::unload_layouts_line(&vm)
                );
                // gcg g1/p: compiled-`new` helper entries -- printed below,
                // outside this GC-stats block (JIT r21 wave 2, lane alloc).
                // Time-to-safepoint; see `ttsp_exit_line`.
                eprintln!("{}", ttsp_exit_line(&vm.mem.gc_barrier.ttsp_stats()));
                // gci i6/jr: the release -> resume census of the barrier's waiters.
                eprintln!("{}", vm.mem.gc_barrier.resume_census_line());
            }
        }
        // LANE W4-B — the nine frame/peer root-audit lines, which used to be
        // emitted from `run()`'s teardown and so were lost on the
        // `System.exit` arm. Three of them are must-be-zero CORRECTNESS
        // signals. See `print_frame_root_audit_reports`.
        print_frame_root_audit_reports();
    }

    // `CRATONVM_DBG=ir-isel` — the instruction selector's process totals.
    //
    // Its own switch, not `jit.method_stats`: these are two different
    // measurements and a run that wants one rarely wants the other. At exit
    // rather than per compile because the population is every method the
    // optimizing tier produced a body for — 850 of them across the two Spring
    // Boot classes this was first measured over — and summing that many stderr
    // lines by hand is how a coverage figure gets mis-transcribed.
    if None::<std::ffi::OsString>.is_some() {
        let (methods, stats) = cratonvm_jit::x64::isel::shadow_totals();
        if methods != 0 {
            eprintln!(
                "[ir-isel] TOTALS methods={methods} {}",
                stats.summary_line()
            );
        }
        let (mir_methods, tiles, mismatches) = cratonvm_jit::ir_lower::mir_totals::read();
        if mir_methods != 0 {
            // `shadow_tiles` / `arm_bytes` / `enc_bytes` are verify mode's
            // sizing of the increment byte equality cannot cover: what the
            // encoder would have written for the tiles it is not allowed to
            // emit, against what the per-opcode arms did write.
            let (shadow, arm_bytes, enc_bytes) = cratonvm_jit::ir_lower::mir_totals::read_shadow();
            eprintln!(
                "[ir-isel] MIR TOTALS methods={mir_methods} tiles={tiles} \
                 mismatches={mismatches} shadow_tiles={shadow} \
                 arm_bytes={arm_bytes} enc_bytes={enc_bytes}"
            );
        }
        // Increment 2's verdict tally. Printed on its own line and with the
        // three states kept apart on purpose: `verified` methods with a zero
        // `values` total and `nothing_to_cover` methods look identical in any
        // collapsed "ok" count, and only the first is evidence. `rejected` is
        // not a ratio — any non-zero value is a compiler bug.
        let (verified, values, vacuous, indescribable, rejected) =
            cratonvm_jit::ir_lower::mir_totals::read_alloc();
        if verified + vacuous + indescribable + rejected != 0 {
            eprintln!(
                "[ir-isel] MIR ALLOC verified={verified} values={values} \
                 nothing_to_cover={vacuous} indescribable={indescribable} \
                 rejected={rejected}"
            );
        }
    }

    // Final tally for the resolved-field site cache. Self-gated on
    // `CRATONVM_DBG=field-site`; a run that never sets it prints nothing. This
    // is what proves the lever is live before anyone times it — an inert gate
    // reports `hit=0` here rather than hiding inside a timing wash.
    cratonvm_vm::runtime::interpreter::site_cache::site_stats::dump();
    cratonvm_vm::runtime::interpreter::invoke_phases::dump();
    cratonvm_vm::runtime::interpreter::field_phases::dump();

    // The G1 live-region memo's tally, self-gated on
    // `CRATONVM_DBG_G1_LIVE_MEMO`. Same argument as the line above, and it is
    // the only usable one for that change: this host has no PMU, so a
    // `perf stat -e instructions` A/B is unavailable, and its load average
    // moves further in an hour than the effect does.
    cratonvm_vm::dump_g1_live_region_memo_stats();

    // The JIT root-scan tally, self-gated the same way. It answers what the
    // method-stats line below cannot: those counters price the COMPILER, and a
    // run where compilation costs 3 ms while the JIT still costs +83% CPU has
    // its cost somewhere the compiler statistics do not reach.
    cratonvm_vm::jit::conservative_roots::scan_prof::dump();

    // The oop-map audit's tally, self-gated the same way
    // (`CRATONVM_DBG_VERIFY_OOP_MAPS`). It answers the one question the
    // moving-young design rests on and that nothing else reports: does the
    // precise map name every live reference in the frames it claims to
    // describe? `never_mapped` is that answer; `below_jit` is the interpreter
    // and native region the map never claimed, split out so it cannot be
    // mistaken for a gap the way the previous oracle's single number was.
    cratonvm_vm::jit::conservative_roots::oop_map_audit::dump();

    // How many native-registry probes one invoke cost, self-gated on
    // `CRATONVM_DBG_NATIVE_LOOKUPS=1`. This is the number
    // `vm-per-call-dispatch-cost-RETIRED-20260817.md` §2 asks for
    // before anyone restructures the dispatch entry points: a profile share can
    // say `slot_for_exact` is 8.5%, but only this says whether a "one lookup
    // per invoke" rewrite would divide it by 1 or by 10.
    cratonvm_native_api::registry::lookup_census::report("exit");

    // Which classes this run DEFINED, hottest first, self-gated on
    // `CRATONVM_DBG=define-census`. Class definition is the only thing that
    // calls `JitCache::invalidate_for_class`, so a profile that shows that
    // symbol long past warm-up is asking this question and nothing else could
    // answer it — `runtime::diagnostics::classes_loaded` reported zero because
    // nothing incremented it.
    cratonvm_classloading::define_census::dump();

    // The `Lookup` finder phase census, self-gated on
    // `CRATONVM_DBG=mh-finder-census` (interpreter round i4 wave 89, lane L4):
    // finder memo hits / refusals by reason, the JDK body's time, the
    // `MethodHandleNatives.resolve` share, and first vs repeat invokes.
    cratonvm_native_builtins::mh_finder_census::dump();

    // The inflater stream census, self-gated on `CRATONVM_DBG=zip-stream-census`
    // (interpreter round i6 wave 116, lane L5): inflate calls and bytes by
    // stream kind and consumer frame, `Resource.getBytes` split out.
    cratonvm_native_builtins::zip_stream_census::dump();

    // The lambda tier-up / inline-cache-thunk census, on
    // `CRATONVM_DBG=lambda-jit`.
    //
    // At exit and not merely periodically, because the periodic report fires
    // every 200 000 eligible dispatches (or 100 000 direct calls) and no
    // ordinary application workload comes near either: a census over 24 Tomcat
    // JUnit classes — 129 s of real work, one of them driving 21 HTTP tests
    // against a live connector — printed nothing whatsoever, and "no lambda
    // activity" is not a reading that silence can support. See
    // `runtime::interpreter::report_lambda_census_at_exit`.
    cratonvm_vm::runtime::interpreter::report_lambda_census_at_exit();
    cratonvm_vm::runtime::interpreter::report_stub_door_tally_at_exit();
    cratonvm_vm::runtime::interpreter::report_native_entry_tally_at_exit();

    // The map-view rebuild-elision census, on `CRATONVM_DBG=map-view-cache`.
    // `resync_skipped` is the ENGAGEMENT counter for the keySet-view fast path:
    // a wall-clock number quoted without it cannot say whether the fast path
    // ran at all.
    cratonvm_vm::report_map_view_cache_at_exit();

    // The stale-frame-word oracle's run totals, on `CRATONVM_DBG=remap-residue`.
    // `local_oop` is the count that names a missed root; the per-frame
    // `stale_live` number it replaces is an upper bound that includes dead
    // spill residue, and was twice read as a verdict. `frames` is the
    // engagement counter. See
    // `jit::conservative_roots::report_remap_residue_census_at_exit`.
    cratonvm_vm::jit::conservative_roots::report_remap_residue_census_at_exit();

    // The interpreter's two root-set censuses, each printing nothing unarmed:
    // the loose operand-stack long roots (`CRATONVM_DBG_LONGROOT`; `unminted=0`
    // is the go signal of
    // `docs/feature-designs/real-jdk-mode.md`)
    // and the precise-oop-map shadow (`CRATONVM_DBG_VERIFY_OOP_MAPS`; `missed`
    // and `extra` decide stage 3 of the precise-interpreter-oop-maps proposal).
    // Their running lines stop at power-of-two totals; these are exact.
    cratonvm_vm::runtime::value_stack::report_longroot_census_at_exit();
    cratonvm_vm::runtime::local_liveness::oop_map_shadow::report_at_exit();

    // The punned-cell watch census, on `CRATONVM_DBG_WATCH_PUN=<class>:<slot>`.
    // `accessor_reads` / `accessor_stores` are the ENGAGEMENT counters: the
    // experiment this watch exists for turns the JIT off, which also removes
    // every compiled read and write of the slot, so a zero numerator without
    // them cannot be told from "nobody looked".
    cratonvm_vm::report_punned_watch_at_exit();

    // The execution profile, on `CRATONVM_PROFILE_SAMPLE_MS`. Prints nothing
    // unarmed, and prints its DENOMINATOR first when armed -- a ranking whose
    // total is unstated invites reading 40% of samples as 40% of the run, and
    // on a workload that spends its time blocked those differ by everything.
    cratonvm_vm::runtime::exec_sampler::report_at_exit();

    // The notification-credit census, on `CRATONVM_DBG=monitor-notify`.
    // `credits_consumed` is the engagement counter for the `Object.wait()`
    // lost-wakeup fix: a run with no stalls says nothing about whether the
    // condition path was exercised, and this is what tells that apart from
    // the switch having been off.
    cratonvm_vm::threading::monitor::report_monitor_notify_census_at_exit();

    // The two composition censuses (`CRATONVM_DBG_INTERP_FRAMES`,
    // `CRATONVM_DBG_TIERUP_DECLINE`). Each prints nothing when unarmed.
    cratonvm_vm::runtime::interp_census::report_at_exit();

    // The IR transform census and the optimizing tier's admission funnel, on
    // `CRATONVM_JIT_TRANSFORM_CENSUS`. Also called from `main`'s exit arms,
    // which a JUnit runner never reaches: it leaves through `System.exit`, and
    // this is the printer that path runs. `Once`-guarded, so a run that takes
    // both prints once.
    cratonvm_jit::ir_evidence::exit_summary();

    // The single-pass nested-splice hit/miss census, on
    // `CRATONVM_DBG_SP_NESTED_SPLICE_CENSUS` (JIT round 19 wave 9, TAIL819-1).
    // Prints nothing unarmed; prints once if both exit arms reach it.
    cratonvm_jit::x64::report_sp_nested_splice_census_at_exit();

    // HANDLER8-1 (JIT round 19 wave 9): the outside-manager C1 -> C2 census, on
    // `CRATONVM_DBG_OUTSIDE_MANAGER_C2_CENSUS` only (since the JIT r23 triage
    // the armed supersede alone no longer prints it). Once per core (a core
    // dropped earlier printed its own line); nothing when unarmed.
    cratonvm_jit::tiered::print_outside_manager_c2_census_at_exit();

    // GPU offload engagement and dispatch censuses (each `Once`-guarded and
    // silent when no GPU work ran). `lang_system::run_shutdown_hooks` calls
    // this in `--compatible` mode; `--real-jdk` runs real `java.lang.Shutdown`
    // bytecode and exits through here instead.
    cratonvm_native_builtins::craton_gpu::dispatch_timing::report();

    // -----------------------------------------------------------------
    // THE COLLECTOR'S ENGAGEMENT NUMBERS
    // -----------------------------------------------------------------
    //
    // Outside `jit.method_stats`, deliberately. Every one of these was written
    // to make a ZERO readable -- `slots=0` cannot distinguish a kill switch
    // from a workload with no static reference fields; `hits=0` cannot
    // distinguish an off bitmap from one that is never consulted; a give-back
    // that returns nothing looks exactly like a give-back that is disabled.
    // Gating them behind `CRATONVM_JIT_METHOD_STATS` reintroduced the failure
    // they exist to prevent, one level up: a reader who sets the GC switch the
    // record names and gets no line reads that as "the switch did nothing", and
    // the flag that would have shown otherwise is about the JIT and is named in
    // none of those records.
    //
    // Each still prints only when its own counter is non-zero, which is a
    // different rule and the right one: these are opt-in instruments, so an
    // unarmed one has nothing to say and should stay quiet.
    //
    // nio21 wave 8 lane L4: the ALWAYS-ON counters among them (object starts,
    // young uncommit, field-by-name drops, static root slots, the statics scan,
    // the young-mark drain) are non-zero on an ordinary probe, and HotSpot
    // prints none of this, so they print under `CRATONVM_DBG=exit-census`
    // (or their own switch where they have one). The opt-in instruments below
    // (root fix-up, mark-bitmap clear, gc-trigger, G1 objaddr) count only when
    // armed and are unchanged.
    // r23 w1 L11: the always-on rows below print through `exit_report!`
    // (`cratonvm_types::exit_report`), the one exit-report policy.
    // EXACT OBJECT-START answers (`CRATONVM_GC_OBJECT_STARTS`). The
    // pair, because a hit count alone cannot say whether the bitmap is
    // carrying the predicate or being consulted and ignored: a miss
    // falls through to the header-shaped deduction that was there
    // before, and on a conservative scan over zeroes, small integers
    // and long bit patterns most candidates SHOULD miss. What the ratio
    // cannot separate -- a genuine non-object from a real object the
    // bitmap never saw because a TLAB bump-allocated it -- needs a
    // workload, not another counter.
    {
        let (hits, misses, missed_objects) = cratonvm_vm::object_start_counts();
        if hits != 0 || misses != 0 {
            cratonvm_types::exit_report!(
                "[cratonvm] exact object-start answers: hits={hits} misses={misses} (of which real objects: {missed_objects})"
            );
        }
    }
    // MEMORY RETURNED TO THE OS by the generational young collector
    // (`CRATONVM_GEN_UNCOMMIT`). Printed whenever a collection ran,
    // zero included: a zero with the switch ON means every collection
    // found the evacuated semi-space had no whole granule to give back,
    // which is a real finding about the workload, and it is
    // indistinguishable from the switch being off unless it is printed.
    {
        let bytes = cratonvm_vm::young_bytes_uncommitted();
        if bytes != 0 {
            cratonvm_types::exit_report!("[cratonvm] generational young uncommit: {bytes} bytes returned to the OS");
        }
    }
    // FIELD-BY-NAME STORES THAT WENT NOWHERE. `NativeContext::set_field_by_name`
    // resolves the field against the class of whatever object is AT the address
    // it is handed, and DROPS the store when it does not resolve. That is a
    // legitimate outcome for a native setting a field only some subclasses
    // declare — and it is also what a stale receiver looks like, because a
    // vacated young address is usually re-served to an unrelated object before
    // the store runs. The drop happens before the heap is touched, so no
    // `[deadref-*]` arm, no `[CELLWATCH]` and no `[PUTFIELD-WATCH]` can see it;
    // this counter is the only place it is visible at all.
    //
    // A non-zero number is not a defect by itself. It is the denominator for
    // `CRATONVM_DBG_DEADREF_STORE=1`, which turns each one into a
    // `[field-by-name-dropped]` line with the receiver's class and a backtrace.
    {
        let dropped = cratonvm_vm::vm::field_by_name_dropped_count();
        // Also under its own per-occurrence switch: whoever asks for the
        // `[field-by-name-dropped]` lines wants the denominator.
        if dropped != 0 {
            cratonvm_types::exit_report!(
                or false;
                "[cratonvm] set_field_by_name stores dropped: {dropped} — the receiver's class did \
                 not declare the named field. Expected for natives that set a field only some \
                 subclasses have; ALSO the shape a stale receiver takes, because the drop happens \
                 before the heap is touched and no stale-reference probe can see it. \
                 CRATONVM_DBG_DEADREF_STORE=1 names each one."
            );
        }
    }
    // STATIC ROOT SLOTS -- the engagement number for the slot-carrying
    // root path (`CRATONVM_GC_STATIC_ROOT_SLOTS`). Both halves, for the
    // usual reason: `slots=0` alone cannot distinguish the kill switch
    // being set, the scan/fix-up pairing being broken so every
    // collection re-walks every static the old way, and a workload that
    // simply has no static reference fields. `fallbacks` tells the
    // first two from the third.
    //
    // Neither number says the recorded list was COMPLETE. Only
    // `CRATONVM_DBG_STATIC_SLOT_VERIFY=1` answers that, and no counter
    // can stand in for it -- a list missing a slot looks identical here.
    {
        let (slots, fallbacks) = cratonvm_vm::memory::roots::static_root_slot_counts();
        if slots != 0 || fallbacks != 0 {
            cratonvm_types::exit_report!(
                "[cratonvm] static root slots: patched={slots} full-walk-fallbacks={fallbacks}"
            );
        }
    }
    // THE STATICS SCAN, always on. Section 2 of `collect_roots` walks
    // every static field of every loaded class on every collection, and
    // the standing proposal is to narrow it by declared type. `slots` is
    // what such a filter would shrink and `objects` is what has to be
    // visited whatever it says, so the pair bounds the saving before
    // anyone argues about whether the filter is safe on a tree with a
    // history of smuggled `jobject`s.
    {
        let (passes, slots, objects, ns) = cratonvm_vm::memory::roots::statics_scan_counts();
        if passes != 0 {
            cratonvm_types::exit_report!(
                "[cratonvm] statics scan: passes={passes} slots={slots} \
objects={objects} total={ms}ms mean={mean}us",
                ms = ns / 1_000_000,
                mean = ns / 1_000 / passes.max(1),
            );
        }
    }
    // POST-COLLECTION ROOT FIX-UP (`CRATONVM_DBG_ROOTFIXUP=1`) -- what
    // the value-carrying root ABI costs per run. The standing proposal
    // is to carry roots as SLOTS so a relocating collector writes the
    // new address through them and this walk disappears; the proposal is
    // worth what the walk costs, and this is that number. Printed with
    // the pointer-map total because the two shapes the cost could have
    // -- proportional to the relocation set, or to the root surface --
    // call for different fixes.
    {
        let (calls, entries, ns) = cratonvm_vm::memory::gc::root_fixup_census();
        if calls != 0 {
            eprintln!(
                "[cratonvm] root fix-up: calls={calls} \
pointer-map-entries={entries} total={ms}ms mean={mean}us",
                ms = ns / 1_000_000,
                mean = ns / 1_000 / calls.max(1),
            );
        }
        // gc-common w5-f (`handoff-w4a-print-the-blocked-fold-census`): the
        // in-pause fold of each relocating pause's pointer map into every
        // BLOCKED thread's fixup chain. It runs INSIDE `update_all_roots`, so
        // the line above already contains this time; this is its share, which
        // `common-w2g-per-gc-remap-costs` §1 found invisible. Only
        // `update_all_roots` folds, so `folds != 0` implies `calls != 0`.
        let (folds, threads, fold_ns) =
            cratonvm_vm::threading::thread_registry::blocked_fold_census();
        if let Some(line) = blocked_fold_exit_line(folds, threads, fold_ns) {
            eprintln!("{line}");
        }
    }
    // MARK-BITMAP CLEAR (`CRATONVM_DBG_MARKCLEAR=1`). `worked` is the
    // engagement half: the change being measured is the `any_marked`
    // early return, so `worked == calls` says it never fired on this
    // workload and the reading is about the clear loop rather than about
    // the change.
    {
        let (calls, worked, words, ns) = cratonvm_vm::mark_bitmap_clear_census();
        if calls != 0 {
            eprintln!(
                "[cratonvm] mark-bitmap clear: calls={calls} worked={worked} \
words={words} total={us}us",
                us = ns / 1_000,
            );
        }
    }
    // The GC-TRIGGER publish (`CRATONVM_DBG_GC_TRIGGER_VERIFY=1`). The
    // pair, because a zero divergence count is equally consistent with
    // "every mutation went through the guard" and with "the verifier
    // never ran", and only the check count tells them apart.
    {
        let (checks, diverged) = cratonvm_vm::gc_trigger_verify_counts();
        if checks != 0 {
            eprintln!("[cratonvm] gc-trigger publish: checks={checks} divergences={diverged}");
        }
    }
    // THE YOUNG-MARK DRAIN, always on. `workers_last` is the line that
    // says whether a worker-count A/B reached this collector at all: the
    // sibling question on G1 turned out to be two INERT levers and one
    // live one, so every earlier A/B on either of the first two had
    // compared a binary against itself.
    {
        let (calls, par, workers, ns) = cratonvm_vm::young_mark_drain_census();
        if calls != 0 {
            cratonvm_types::exit_report!(
                "[cratonvm] young-mark drain: calls={calls} parallel={par} \
workers_last={workers} total={ms}ms",
                ms = ns / 1_000_000,
            );
        }
    }
    // G1's `is_object_address` (`CRATONVM_DBG_G1_OBJADDR=1`). The
    // question is whether G1 should get the exact object-start bitmap
    // the generational predicate got, and the answer is a share of a run
    // rather than a per-call cost -- see the census's own doc.
    {
        let (calls, accepted, ns) = cratonvm_vm::g1_object_address_census();
        if calls != 0 {
            eprintln!(
                "[cratonvm] g1 is_object_address: calls={calls} \
accepted={accepted} total={ms}ms",
                ms = ns / 1_000_000,
            );
        }
    }

    // The per-SITE half of the same question, and the reason it once took a
    // sampled-backtrace build to answer: the per-thread walk tallies have counted
    // every JIT-helper membership walk since it was added, and
    // `membership_walks_by_site()` -- its only reader -- had NO CALLER at all.
    // A write-only counter is a diagnosis nobody can read; the census's own doc
    // says `perf` cannot attribute this population on an optimized build (DWARF
    // returns self-recursive frames, LBR is unavailable on the virtualised PMU),
    // which is exactly why the counters exist. They are ungated and already
    // paid for, so the only thing missing was this line.
    //
    // It rides the same gate as the total above -- a non-zero census -- rather
    // than printing unconditionally: whoever asks for the TOTAL is the one who
    // wants the split, and re-reading the env var here would be a second
    // spelling of the same switch that could drift from it. The JIT-helper
    // sites are a MINORITY of that total -- the
    // sampled population on `org.h2.test.store.TestMVStoreTool`'s create phase
    // was 57% `VmHeap::load_and_forward` (the software read barrier, one walk
    // per reference field/array access) and 17% `G1Collector::autobox_payload`
    // -- so a small number here is the useful answer, not a broken instrument.
    {
        let sites = cratonvm_vm::jit::helpers::membership_walks_by_site();
        if !sites.is_empty() && cratonvm_vm::g1_object_address_census().0 != 0 {
            let total: u64 = sites.iter().map(|(_, v)| *v).sum();
            let body = sites
                .iter()
                .map(|(n, v)| format!("{n}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            eprintln!(
                "[cratonvm] jit-helper membership walks: total={total} {body} -- the \
JIT-helper SUBSET of the is_object_address line above; the remainder is the read \
barrier and the array/field accessors, which carry no site counter."
            );
        }
    }

    // The workspace's otherwise-unread instruments, on their OWN switch
    // (`CRATONVM_INSTRUMENT_CENSUS=1`) and deliberately not folded into
    // `jit.method_stats` below: it spans eight crates, most of them nothing to
    // do with the JIT, and a reader who asked for method statistics did not ask
    // for the GC's block-offset census. Every row is a counter that no other
    // report reads; the module doc says what that does and does not prove.
    // Round 10, closing `r10-diagread-orphan-instrument-debt-register`.
    cratonvm_vm::runtime::instrument_census::dump_to_stderr();

    if cratonvm_types::flags().jit.method_stats {
        cratonvm_jit::tiered::dump_method_stats_to_stderr();
        // The `getfield` fast-path ENGAGEMENT number, on the same switch. The
        // guarded inline `getfield` is emitted at dozens of sites and can still
        // never take its inline branch; only this counter distinguishes
        // "emitted" from "ran". Pair it with the compile-time
        // `[compact-inline] MISS` census under CRATONVM_DBG_COMPACT_INLINE:
        // MISS names the SITES that cannot inline, this names the ACCESSES that
        // paid the helper's `is_object_address` walk. See
        // every-jit-getfield-takes-the-helper-FIXED-20260820.md.
        eprintln!(
            "[cratonvm] getfield helper calls: {} (of which trusted-ref: {}) | CALL sites emitted by arm: {}",
            cratonvm_vm::jit::helpers::jit_getfield_helper_calls()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<not counted>".to_string()),
            cratonvm_vm::jit::helpers::jit_getfield_trusted_ref_calls(),
            cratonvm_jit::metrics::getfield_arm_emits()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // Reference-store barrier gating. BOTH numbers, always: a zero on the
        // left alone cannot distinguish "this collector published no barrier
        // plan" from "this workload compiled no reference stores", and those
        // call for opposite next steps. `gated` counts sites that got the
        // inline SATB/post-barrier gate sequence; `declined` counts sites that
        // asked and kept the full `jit_putfield_object` path.
        //
        // The switch this replaces measured nothing:
        // `CRATONVM_NO_JIT_INLINE_PUTFIELD` was a no-op under the default
        // collector because the path it disabled was already unreachable
        // (`region_bounds_are_live` is false under G1 and ZGC).
        {
            // G1's inline post-write barrier (F-08, CRATONVM_G1_INLINE_BARRIER).
            // Sites always; the run-time pair only under the ref-store trace.
            // `sites=0` is the expected reading on every other collector and
            // under the default configuration, which is exactly why it is
            // printed rather than inferred.
            {
                let (sites, skipped, called) = cratonvm_jit::metrics::g1_inline_barrier_counts();
                if sites != 0 {
                    eprintln!("[cratonvm] G1 inline post-write barrier: sites={sites}");
                    if skipped != 0 || called != 0 {
                        eprintln!(
                            "[cratonvm]   G1 barrier executions: skipped={skipped} called={called}"
                        );
                    }
                }
            }
            // TLAB object SHAPES. The pair plus the bytes, because "compact=0"
            // means either that `CRATONVM_COMPACT_TLAB_ALLOC` is off or that no
            // allocated class has a registered layout, and those are different
            // facts -- while the saving is the only number that says whether
            // the change was worth making.
            {
                let (compact, legacy, saved) =
                    owning_vm.map_or((0, 0, 0), |vm| {
                        cratonvm_vm::runtime::interpreter::tlab_object_shape_counts(vm)
                    });
                if compact != 0 || legacy != 0 {
                    eprintln!(
                        "[cratonvm] TLAB object shapes: compact={compact} legacy={legacy} \
bytes-saved={saved}"
                    );
                }
            }
            let (gated, declined) = cratonvm_jit::x64::ref_store_site_counts();
            eprintln!("[cratonvm] compiled reference stores: gated={gated} declined={declined}");
            // zt t12/ca4: the ZGC allocation spill sink's compile-time
            // engagement (`CRATONVM_JIT_ZGC_ALLOC_SPILL_SINK`). Printed at 0
            // too: its runtime footprint is meant to be nil, so only this
            // count says whether an A/B arm compiled any sunk site at all.
            eprintln!(
                "[cratonvm] zgc alloc spill sink sites: {}",
                cratonvm_jit::x64::zgc_alloc_spill_sink_site_count()
            );
            // What the DECLINED sites cost, only under the trace. Printed
            // separately from the gated pair because it is the number that was
            // missing: `declined=N` with no execution line reads exactly like a
            // workload that never stored a reference.
            {
                let full = cratonvm_jit::metrics::ref_store_full_helper_count();
                if full != 0 {
                    eprintln!("[cratonvm]   ref-store full-helper executions: {full}");
                }
            }
            // The DYNAMIC split for that pair, only under
            // CRATONVM_DBG_SP_REF_STORE_TRACE=1. `gated=N` above counts emitted
            // sequences; this counts executions, and on the optimizing tier the
            // two turned out to differ by everything -- `gated=2` sitting on
            // `inline=0` out of 16.4M. `barrier` is a SUBSET of `inline`: those
            // stores happened inline and then still called the collector's own
            // write barrier.
            let (sp_inline, sp_barrier, sp_helper) =
                cratonvm_jit::metrics::sp_ref_store_path_counts();
            if sp_inline != 0 || sp_helper != 0 {
                eprintln!(
                    "[cratonvm]   ref-store executions: inline={sp_inline} \
(of which barriered={sp_barrier}) helper={sp_helper}"
                );
                for (name, count) in cratonvm_jit::metrics::sp_ref_store_bails() {
                    eprintln!("[cratonvm]     ref-store bail {name}: {count}");
                }
            }
            // The two arms the gated one falls through to. Printed even when
            // the gated pair above is all zeros, because that combination is
            // exactly the reading that looked like a hole on bt18 under the
            // generational collector: `gated=2` emitted, nothing executed, and
            // four hot stores being served inline by an arm no census knew
            // about.
            let (fresh_ctor, body) = cratonvm_jit::metrics::sp_ref_store_other_arm_counts();
            if fresh_ctor != 0 || body != 0 {
                eprintln!(
                    "[cratonvm]   ref-store executions, other inline arms: \
fresh-ctor={fresh_ctor} body={body}"
                );
            }
            // zt t9/ca (Q4): the single-pass `aastore`'s ZGC SATB pre arm,
            // only under the same trace. `entered - called` is the number of
            // null old elements whose call
            // `CRATONVM_JIT_ZGC_SP_AASTORE_NULL_OLD_SKIP=1` skipped.
            let (pre_entered, pre_called) = cratonvm_jit::metrics::sp_aastore_satb_pre_counts();
            if pre_entered != 0 {
                eprintln!(
                    "[cratonvm]   sp aastore satb-pre executions: entered={pre_entered} \
called={pre_called}"
                );
            }
            // The OPTIMIZING tier's own pair, never folded into the one above.
            // Until 2026-09-02 that tier lowered every reference store to the
            // helper unconditionally, so it reported neither number -- and a
            // hot loop is compiled there, which is why a healthy single-pass
            // count said nothing about where the time went. Zero on BOTH sides
            // here means the tier compiled no reference store at all; a
            // declined count with the reasons below means it asked and refused.
            let (ir_gated, ir_declined) = cratonvm_jit::metrics::ir_ref_store_site_counts();
            eprintln!(
                "[cratonvm] optimizing-tier reference stores: gated={ir_gated} declined={ir_declined}"
            );
            for (name, count) in cratonvm_jit::metrics::ir_ref_store_declines() {
                eprintln!("[cratonvm]   ir ref-store declined {name}: {count}");
            }
            // The DYNAMIC split, only under CRATONVM_DBG_IR_REF_STORE_TRACE=1.
            // `gated=N` above is a count of emitted sequences; this is a count
            // of executions, and a sequence whose compactness gate never
            // passes has the first without the second.
            let (inline_taken, helper_taken) = cratonvm_jit::metrics::ir_ref_store_path_counts();
            if inline_taken != 0 || helper_taken != 0 {
                eprintln!(
                    "[cratonvm]   ir ref-store executions: inline={inline_taken} helper={helper_taken}"
                );
                for (name, count) in cratonvm_jit::metrics::ir_ref_store_bails() {
                    eprintln!("[cratonvm]     ir ref-store bail {name}: {count}");
                }
                // WHICH shape those inline stores wrote. The legacy arm exists
                // because compact receivers used to be rare; the compact TLAB
                // shape is the default since 2026-09-04, so this pair is what
                // says whether that arm still carries anything.
                let (shape_c, shape_l) = cratonvm_jit::metrics::ir_ref_store_shape_counts();
                if shape_c != 0 || shape_l != 0 {
                    eprintln!(
                        "[cratonvm]     ir ref-store shapes: compact={shape_c} legacy={shape_l}"
                    );
                }
            }
            // Optimizing-tier allocation. A zero on the left is the EXPECTED
            // reading under a default configuration -- `c2_alloc_upgrade` is
            // opt-in, so no method containing a `new` reaches that tier -- and
            // printing both is what separates that from "emitted and refused".
            let (bump, stub) = cratonvm_jit::runtime_lowering::ir_alloc_site_counts();
            eprintln!(
                "[cratonvm] optimizing-tier allocations: inline-bump={bump} stub-only={stub}"
            );
            // getfield receiver null checks. The PAIR, never the ratio: an
            // all-zero pair means the trusted-oop arm was never reached at all
            // (no inline getfield compiled), while zero-elided-with-nonzero-
            // emitted means it WAS reached and the dataflow proved nothing.
            // Those look identical as a percentage and want opposite fixes.
            let (nn_elided, nn_emitted) = cratonvm_jit::x64::receiver_null_check_counts();
            let nn_implicit = cratonvm_jit::x64::receiver_null_check_implicit_count();
            let (nn_i1, nn_i2) = cratonvm_jit::x64::receiver_null_check_implicit_by_arm();
            eprintln!(
                "[cratonvm] getfield receiver null checks: elided={nn_elided} \
                 implicit={nn_implicit} (compact-arm={nn_i1} legacy-arm={nn_i2}) \
                 emitted={nn_emitted}"
            );
            // Implicit null-check table. All four, because no one of them is a
            // verdict: `registered` alone cannot separate "off" from "on and
            // nothing qualified"; `recovered` alone cannot separate "no null
            // receiver occurred" from "the entry was missing"; and a non-zero
            // `declined` is the reading that says the table filled up and the
            // feature has stopped applying to new compiles.
            // The optimizing tier's own receiver null checks, never folded
            // into the single-pass pair above: the two tiers prove the fact by
            // different routes, and folding them would hide a tier that had
            // stopped proving it at all.
            let (ir_seed, ir_el, ir_em) = cratonvm_jit::metrics::ir_receiver_null_check_counts();
            eprintln!(
                "[cratonvm] optimizing-tier receiver null checks: seeded={ir_seed} \
                 elided={ir_el} emitted={ir_em}"
            );
            let (in_reg, in_ret, in_rec, in_dec) = cratonvm_jit::implicit_null::counts();
            eprintln!(
                "[cratonvm] implicit null checks: registered={in_reg} retired={in_ret} \
                 recovered={in_rec} declined={in_dec}"
            );
            // The three implicit-null counters that are NOT part of the
            // four-tuple, printed on the same switch and — like the tuple —
            // printed even when every one of them is zero.
            //
            // Zero is the reading each of them is FOR. They are the module's
            // own early-warning instruments for the two correctness properties
            // `jit/src/implicit_null.rs` calls the point of its design, and an
            // instrument nothing reads cannot warn anyone: until this line
            // landed, `grep -rn 'stale_duplicates_retired\|unlinked_chain_nodes'`
            // found only the definitions, the module prose and one test
            // (`docs/feature-designs/jit-ir-optimizations.md`).
            //
            // * `stale-duplicates-retired` should read zero FOREVER. Non-zero
            //   means `Table::insert` met a live entry at a PC it was
            //   registering with a DIFFERENT recovery address — i.e. some drop
            //   path freed a code buffer without calling
            //   `implicit_null::unregister_range`, which is hazard 2 (a stale
            //   recovery entry outliving its buffer) already happening. The
            //   insert repairs the table; this number is the only thing that
            //   says it had to.
            // * `chain-nodes-unlinked` is read BESIDE `retired` above, never on
            //   its own. It counts dead retirement-chain nodes spliced out
            //   since 2026-09-16. A `retired` that climbs while this stays flat
            //   is the signature of the splice having stopped, which silently
            //   returns code-buffer retirement to the O(table) sweep the page
            //   chains replaced — a performance regression with no functional
            //   symptom, so nothing else would catch it.
            //
            //   Read the two LEFT-TO-RIGHT, in that order, and the reason is
            //   that they overlap. `Table::insert`'s stale-duplicate repair
            //   bumps `retired` WITHOUT walking a chain, so it cannot bump
            //   `unlinked`; a non-zero `stale-duplicates-retired` therefore
            //   produces exactly the "retired climbing, unlinked flat" shape
            //   that otherwise means the splice has stopped. Subtract the
            //   first from `retired` before reading the second. Printing them
            //   on one line is what makes that subtraction possible at all,
            //   and is why this is one line rather than three.
            // * `entry-not-base` counts `note_registration_base` mismatches:
            //   compiles refused because the method entry was not the mapping
            //   base. Non-zero is a REFUSED COMPILE, not a crash — the driver
            //   bails with `implicit-null-entry-not-base` and installs nothing,
            //   which is the fail-closed direction. But a steady stream of them
            //   means the feature has silently turned itself off for every
            //   method carrying an implicit-null site, and the only other
            //   instrument for that is a `debug_assert` that is compiled out of
            //   the release binaries this line runs in. A non-zero reading here
            //   is a prologue that moved, and is answered by reading
            //   `jit/src/implicit_null.rs`'s "The invariant that arrives through
            //   the door nobody checks" before touching anything else.
            let in_stale = cratonvm_jit::implicit_null::stale_duplicates_retired();
            let in_unlinked = cratonvm_jit::implicit_null::unlinked_chain_nodes();
            let in_basemiss = cratonvm_jit::implicit_null::base_mismatches();
            eprintln!(
                "[cratonvm] implicit null-check table health: \
                 stale-duplicates-retired={in_stale} (expected 0) \
                 chain-nodes-unlinked={in_unlinked} (read beside retired={in_ret}) \
                 entry-not-base={in_basemiss} (expected 0)"
            );
            // The three single-pass intrinsic BIND censuses, on the same switch
            // and printed even when all three are zero — same argument as the
            // line above, and each of these counters' own doc comment already
            // makes it. `ATOMIC_INTRINSIC_SITES`: "a perf claim about this
            // family is not believable without checking that this is non-zero —
            // the intrinsic answering the same values as the native it replaced
            // proves nothing about whether it actually ran."
            // `OBJECT_HASHCODE_STRING_GUARD_SITES`: "so 'did this land' is
            // answerable without a timing run." Neither was answerable: until
            // this line landed, `rg -nw` on all three names found declarations,
            // `fetch_add`s, doc comments and one line of the statics ratchet's
            // inventory, and no `.load(` anywhere
            // (`docs/feature-designs/jit-ir-optimizations.md`).
            //
            // ONE LINE for the three, because a bind count is only meaningful
            // beside the counts it should be compared with:
            //
            // * `atomic` and `atomicLong` are the SAME bind decision taken
            //   twice. Both matchers are reached from one `match` on the
            //   receiver class name, both always emit an exact-class-id guard
            //   (neither `AtomicInteger` nor `AtomicLong` is final, so a
            //   subclass could override the RMW), and each has its own
            //   off-switch. The reading that matters is therefore the SPLIT:
            //   a workload that binds `atomic` and not `atomicLong` has either
            //   no `AtomicLong` traffic or a broken second matcher, and a
            //   combined total that moves cannot tell those apart. Folding them
            //   is the mistake the `[cratonvm] JIT checkcast inline sites:`
            //   line in `tiered.rs` was split by cause to avoid.
            // * `Object.hashCode-string-guard` is on the same line as the
            //   member of this set whose zero is most easily MISREAD, and the
            //   misreading is the expensive one. That bind is SPECULATIVE: a
            //   site is bound only when its receiver profile is String-dominant
            //   (`receiver_profile_rejects_guard` declines below
            //   `MIN_GUARDED_RECEIVER_EVIDENCE` samples) and only when the
            //   speculation has not already been despecialised by a
            //   `ReceiverTypeChanged` deopt. So zero means "no site had enough
            //   String-dominant profile evidence", NOT "the guard is off" —
            //   `CRATONVM_JIT_OBJECT_HASHCODE_STRING` defaults ON, so the flag
            //   is almost never the explanation. Printed next to two binds that
            //   are NOT profile-gated, the difference is legible: two non-zeros
            //   and a zero says profiling, three zeros says the intrinsic pass
            //   did not run on this workload at all.
            //
            // Counted at EMISSION, not in the matcher (review #80), so a
            // code-buffer retry that re-emits a method counts its sites again.
            // These are upper bounds on distinct source sites and are compared
            // against zero and against each other, never divided.
            // The BOX_UNBOX pair joins this line rather than getting one of its
            // own, and that is not crowding — it is what the counters' own doc
            // asks for. `LONG_LONG_VALUE_INTRINSIC_SITES`' doc comment states
            // the acceptance criterion by citing these exact siblings: "the same
            // rule the two `Atomic*` counters beside this one state: 'the
            // intrinsic answering the same values as the native it replaced
            // proves nothing about whether it actually ran.' A boxing perf claim
            // quoting a ns/op without a non-zero site count here is quoting a
            // number from a run where nothing was intrinsified." Five counters,
            // one criterion, one line — and a reader comparing a boxing claim
            // against an atomic claim does it without scrolling.
            //
            // `box_unbox_intrinsic_sites()` also had ZERO callers workspace-wide
            // until this line, so both of its counters were written at emission
            // by `jit/src/x64/op_invoke.rs` and read by nothing — which made the
            // acceptance criterion its own doc states unevaluable by anyone
            // (`docs/feature-designs/jit-ir-optimizations.md`).
            //
            // Tuple order is `(Long.longValue, Integer.intValue)` — LONG FIRST,
            // which is the opposite of the reading order of the line's text, so
            // it is destructured into named bindings rather than used positionally.
            let (ai_sites, al_sites, ohc_sites) = cratonvm_jit::intrinsic_site_census();
            let (bu_long, bu_int) = cratonvm_jit::box_unbox_intrinsic_sites();
            eprintln!(
                "[cratonvm] JIT intrinsic binds: atomic={ai_sites} \
                 atomicLong={al_sites} \
                 Integer.intValue={bu_int} Long.longValue={bu_long} \
                 Object.hashCode-string-guard={ohc_sites} (0 = no String-dominant \
                 receiver profile, NOT a disabled flag)"
            );
            // THE OSR EMPTY-OPERAND-STACK RULE, and the two runtime rows that
            // bound it, on one line because the reading is the COMPARISON — a
            // comparison whose terms sit hundreds of lines apart in one stderr
            // dump is a procedure nobody performs.
            //
            // COMPILE-time, unlike the two runtime rows beside it: it counts
            // entry pcs not published, not entries not taken. Reaching it at all
            // needed a `pub use` in `jit/src/x64.rs` — `mod osr` is private, and
            // `lib.rs` is that module's ancestor rather than its parent, so no
            // crate-root re-export could see it
            // (`r10-producers-osr-empty-stack-refusals-is-unreachable-outside-x64-20260922.md`).
            {
                let osr_empty_stack = cratonvm_jit::x64::osr_empty_stack_refusals();
                let rows = cratonvm_jit::metrics::osr_counts();
                let row = |name: &str| {
                    rows.iter()
                        .find(|(n, _)| *n == name)
                        .map(|(_, c)| *c)
                        .unwrap_or(0)
                };
                eprintln!(
                    "[cratonvm] JIT OSR empty-operand-stack rule: entry-pcs-refused={osr_empty_stack} (COMPILE-time; read against osr_entered={} osr_refused_entry={} from the OSR lifecycle line, which are RUNTIME: refused>0 with osr_entered=0 is over-refusal, refused=0 with osr_entered>0 means the rule was inert here or CRATONVM_JIT_NO_OSR_EMPTY_STACK_ENTRY=1 is set — this line cannot tell those two apart)",
                    row("osr_entered"),
                    row("osr_refused_entry"),
                );
            }
            // THE INSTRUCTION-STREAM BARRIER COUNT, printed as a NAMED
            // UNAVAILABLE rather than as a number, which is the whole reason it
            // is here at all.
            //
            // Wave 6 deliberately did NOT wire this: the accessor returned
            // `u64`, the counter is never written on x86-64, and a reader would
            // have printed a structurally-zero line — indistinguishable from "no
            // barrier was ever issued", which is worse than leaving it unread.
            // Wave 8 changed the accessor to `Option<u64>`, returning `None` on
            // x86-64, and that is what makes the line honest: `None` says the
            // target has no such counter, which is a different statement from
            // `Some(0)`. Printed with an explicit match, not `{:?}`, so the text
            // and the value cannot disagree.
            match cratonvm_jit::platform::instruction_stream_barriers_issued() {
                Some(n) => eprintln!(
                    "[cratonvm] JIT instruction-stream barriers issued: {n} (0 here means no code was published since startup, NOT that publication skips the barrier)"
                ),
                None => eprintln!(
                    "[cratonvm] JIT instruction-stream barriers issued: n/a on this target — x86-64 publication needs no explicit barrier, so nothing counts one. Not a zero reading."
                ),
            }
            // THE `Unsafe` ACCESSOR CENSUS — three bind counts and the
            // execution pair, on one line because the reading is the COMPARISON.
            //
            // `cratonvm_jit::unsafe_accessor_census()` had zero callers anywhere
            // in the workspace, while every one of its five counters has a
            // production feeder: `UNSAFE_ACCESSOR_SITES_SP` from the single-pass
            // walk and `UNSAFE_ACCESSOR_SITES_IR` from the optimizing lowering
            // (both `jit/src/lib.rs`), `UNSAFE_ACCESSOR_SITES_OSR` from the OSR
            // door (`vm/src/runtime/interpreter/jit_bridge.rs`), and
            // `UNSAFE_ACCESSOR_SERVED`/`_DECLINED` from eight sites in
            // `vm/src/jit/helpers.rs`. So this was fed-but-unread — the exact
            // shape of `varhandle_write_direct_helper_sites` further down, and
            // the reason `scripts/check-orphan-instruments.sh` was red on
            // arrival at this wave with this one name and no allowlist entry.
            //
            // **Not allowlisted, deliberately.** That gate's failure message
            // offers `--update-allowlist`, and taking the offer here would have
            // frozen a WORKING instrument as permanent debt — the allowlist's own
            // header calls itself a debt register and not a list of acceptable
            // things. An instrument with five live feeders and no reader wants a
            // reader.
            //
            // ONE LINE, and the three doors kept apart, for the reason the
            // VarHandle families' lines give: `served`/`declined` count
            // EXECUTIONS and the three site counts are BINDS per compile door,
            // so a non-zero bind with `served=0` is the specific failure worth
            // naming (the site compiled and every execution refused), and
            // `served=0 declined=0` with all three binds at zero is a workload
            // with no `Unsafe` accessor traffic — two readings a combined total
            // cannot separate. The doors are split because a hot method only ever
            // runs the optimizing body, so `ir=0` beside a positive `singlepass=`
            // says the tier-up never reached these sites, which is a different
            // finding from the feature being off.
            let (ua_sp, ua_ir, ua_osr, ua_served, ua_declined) =
                cratonvm_jit::unsafe_accessor_census();
            eprintln!(
                "[cratonvm] JIT Unsafe accessor binds: singlepass={ua_sp} optimizing={ua_ir} \
                 osr={ua_osr} served={ua_served} declined={ua_declined} (binds are per compile \
                 door; served/declined count EXECUTIONS, so a positive bind with served=0 is a \
                 site that compiled and refused every call)"
            );
            // THE `Unsafe` COMPARE-AND-SET CENSUS — the same five readings for
            // `compareAndSet{Int,Long}`, on their own line because the CAS is
            // the hot member (every AQS acquire/release from compiled code) and
            // its `served` must not be readable only as part of a family total.
            // `declined` counts shapes the native serves on another arm (null
            // base, array, synthetic offset, an untagged legacy slot) and every
            // call while `CRATONVM_DBG_AQS_TRACE` is on.
            let (uc_sp, uc_ir, uc_osr, uc_served, uc_declined) = cratonvm_jit::unsafe_cas_census();
            eprintln!(
                "[cratonvm] JIT Unsafe CAS binds: singlepass={uc_sp} optimizing={uc_ir} \
                 osr={uc_osr} served={uc_served} declined={uc_declined} (compareAndSet{{Int,Long}}; \
                 CRATONVM_JIT_UNSAFE_CAS_DIRECT=0 turns the bind off)"
            );
            // THE SINGLE-PASS `instanceof` SITE CENSUS — the twin of the
            // `[cratonvm] JIT checkcast inline sites:` line that
            // `cratonvm_jit::tiered::dump_method_stats_to_stderr` prints a few
            // screens above this one, and which until now had no counterpart.
            //
            // `jit/src/x64/op_object.rs` handles `checkcast` and `instanceof` a
            // hundred lines apart, in one file, with the same inline fast-path
            // structure — its own comment says so ("inline fast path:
            // checkcast's guards, instanceof's answers"). The `checkcast` arm
            // fed five counters split by cause and the `instanceof` arm fed
            // none, and the arm's own justification for that ("the checkcast
            // counters describe checkcast, and this arm adds no state") was
            // defensible only while neither arm's numbers reached anyone. They
            // always reached someone: the checkcast five are printed. So an
            // operator got a cause-by-cause account of one type check and
            // nothing at all about its twin — a checkcast-shaped hole, which
            // reads as "instanceof has no inline path" rather than as "nobody
            // counted it"
            // (`docs/feature-designs/real-jdk-mode.md`).
            //
            // SPLIT BY ARM AND BY CAUSE, matching the checkcast line term for
            // term, because the buckets want different fixes: an unresolved
            // target id is transient and a recompile clears it, while an
            // untrusted operand is structural (the backend's `stack_oop_marks`
            // could not prove the operand is an oop) and is the same open
            // question the compact `getfield` arm's diagnostic exists to
            // answer. A single "not inlined" total that moves names neither.
            // The two refusal buckets together are the sites that fell through
            // to `jit_instanceof` with no inline arm at all — the DENOMINATOR,
            // without which a zero on the inline buckets cannot be told from an
            // `instanceof`-free workload.
            //
            // `final-miss=` is the one bucket with no checkcast counterpart and
            // is the reason that page was filed rather than shrugged at.
            // `CRATONVM_JIT_INSTANCEOF_FINAL_MISS` is DEFAULT ON and answers
            // `x instanceof <final class>` with an inline `0` for a receiver of
            // any other class; whether it engages at all depends on
            // `instanceof_miss_is_final_boot_class` matching a hard-coded
            // nine-name list, or on the compile door having proved the target
            // `final`. A performance feature that is on by default, whose
            // engagement is decided by a name-matching predicate, and whose
            // engagement nothing records, is one that cannot be shown to have
            // ever run. It is a SUBSET of `class-id=`, not a disjoint bucket —
            // the same sites, extended with the definite-miss stub — so within
            // each door the non-subset buckets still sum to the sites that door
            // walked, and `final-miss` is read as a fraction of that door's
            // `class-id`.
            //
            // Printed even when every figure is zero, on the same switch as
            // every line above, and the line says why an all-zero reading is
            // ambiguous rather than leaving a reader to assume: both arms,
            // census included, sit under `checkcast_inline_enabled()`, so
            // `CRATONVM_JIT_CHECKCAST_INLINE=0` zeroes all nine exactly as it
            // zeroes the checkcast five. The flag's state separates that from a
            // workload with no `instanceof`; the counters cannot.
            //
            // BOTH DOORS, ON ONE LINE, SPLIT BY DOOR — the caveat this line used
            // to carry ("single-pass door only; the optimizing tier's own arm is
            // not counted") is gone because the gap it described is closed, and a
            // caveat that outlives its gap is the next reader's wrong number.
            // The optimizing tier has its own `instanceof` fast path
            // (`ir_lower.rs`'s `Op::InstanceOf`, round 9 wave 9) and a hot method
            // only ever runs that one — which is precisely why the checkcast
            // counters keep `CHECKCAST_INLINE_SITES_IR` apart from
            // `CHECKCAST_INLINE_SITES`, and why these two halves are compared
            // rather than summed. A zero on the single-pass half beside a moving
            // optimizing half is the EXPECTED reading for a workload whose hot
            // methods all tiered up, not a finding.
            //
            // THE OPTIMIZING HALF HAS FOUR BUCKETS, NOT FIVE, and the line says
            // which one is missing rather than leaving a reader to notice the
            // asymmetry and guess. `refused-untrusted-operand` is fed by the
            // single-pass backend's `operand_is_trusted_oop` (a test over its
            // `stack_oop_marks` abstract-stack marks); the optimizing tier's
            // operand is an SSA value whose reference-ness is a property of the
            // IR, so that population is empty by construction. Printing a
            // same-named bucket fed by a different condition would be worse than
            // printing none.
            //
            // AND `optimizing[prim-array]` IS A NAMED ZERO, said in the output
            // for the same reason the four-vs-five asymmetry is. MEASURED
            // 2026-09-22, not reasoned: a probe with `o instanceof int[]` and one
            // with `o instanceof String[]` both print
            //
            //   [ir] IrBuilder::build refused at ir.rs:11132 (bytecode pc 24)
            //   [ir] IrBuilder::build returned None for ... — no IR body
            //
            // while the same probe's `o instanceof String` method compiles IR and
            // moves `optimizing[class-id]`. `jit/src/lib.rs` admits a site to
            // `instanceof_info` only when `cp_new_resolver` answers
            // `JitNewSite::Resolved`, and an ARRAY-typed `CONSTANT_Class` never
            // does, so `ir.rs`'s `0xc1` arm takes its miss branch,
            // `plant_uncommon_trap` refuses, and the WHOLE method bails to
            // single-pass. The emitting arm is real and
            // `jit/tests/r10_producers_ir_instanceof_site_census.rs` step 3
            // exercises it; nothing in production reaches it. So the reading an
            // operator would otherwise draw from `single-pass[prim-array=2]`
            // beside `optimizing[prim-array=0]` — that one door lost sites the
            // other kept — is wrong, and the line says so. Filed as
            // `docs/feature-designs/jit-ir-optimizations.md`;
            // when that is fixed this clause comes out and the bucket becomes a
            // number, with no other change here.
            let (io_prim, io_cid, io_final_miss, io_no_target, io_untrusted) =
                cratonvm_jit::instanceof_inline_sites();
            let (ir_prim, ir_cid, ir_final_miss, ir_no_target) =
                cratonvm_jit::instanceof_ir_inline_sites();
            eprintln!(
                "[cratonvm] JIT instanceof inline sites: single-pass[prim-array={io_prim} \
                 class-id={io_cid} of-which-final-miss={io_final_miss} \
                 refused-no-target-id={io_no_target} refused-untrusted-operand={io_untrusted}] \
                 optimizing[prim-array={ir_prim} class-id={ir_cid} \
                 of-which-final-miss={ir_final_miss} refused-no-target-id={ir_no_target}] \
                 (all nine are zero when CRATONVM_JIT_CHECKCAST_INLINE=0, which gates both \
                 arms; the refused= buckets are the sites that took the jit_instanceof \
                 helper with no inline arm, i.e. each door's denominator; \
                 of-which-final-miss is a SUBSET of that door's class-id and is the \
                 engagement number for CRATONVM_JIT_INSTANCEOF_FINAL_MISS, which is \
                 default ON; the two doors are independent emitters and a hot method \
                 only runs the optimizing one, so compare them, never add them; \
                 optimizing has no refused-untrusted-operand because that refusal is a \
                 single-pass stack_oop_marks test with no analogue in the IR; \
                 optimizing[prim-array] is a NAMED zero rather than an unread \
                 counter — an array-typed instanceof target bails the whole method \
                 out of the optimizing tier before that arm is reached, so nothing \
                 in production can move it, and a non-zero single-pass prim-array \
                 beside it is that and not a discrepancy)"
            );
            // The `final`-devirtualisation door's native-shadow screen, printed
            // BESIDE the door's own engagement counter because
            // `FINAL_DEVIRT_NATIVE_SHADOW_REFUSED`'s doc comment instructs
            // exactly that and nothing was carrying it out:
            //
            //   "Read it beside [`FINAL_INVOKEVIRTUAL_PINNED`]: together they
            //    say how much of the door's candidate set the screen takes. A
            //    zero here on a workload that moves the sibling counter means
            //    the screen is inert for that workload, not that it is
            //    unnecessary."
            //
            // That reading was not performable. `final_invokevirtual_pinned()`
            // is printed by `tiered.rs`'s `JIT invokevirtual pinned non-virtual:`
            // line (two fields, private and final); `final_devirt_native_shadow_refused_count()`
            // had ZERO callers workspace-wide, and it is `#[must_use]` — a
            // `#[must_use]` nothing uses. So the "beside" half of a documented
            // two-number reading simply did not exist
            // (`docs/feature-designs/jit-ir-optimizations.md`).
            //
            // `final=` is reprinted here rather than cross-referenced, and the
            // duplication with `tiered.rs` is deliberate: the doc's procedure is
            // a COMPARISON, and a comparison whose two terms are hundreds of
            // lines apart in one stderr dump is a procedure nobody performs. The
            // screen's number is meaningless alone — a zero means "inert on this
            // workload" only if the sibling moved, and "the door never ran"
            // otherwise, and those want opposite next steps.
            let fd_refused = cratonvm_jit::final_devirt_native_shadow_refused_count();
            let fd_pinned = cratonvm_jit::final_invokevirtual_pinned();
            eprintln!(
                "[cratonvm] JIT final-devirt native-shadow screen: refused={fd_refused} \
                 (read against final-invokevirtual-pinned={fd_pinned}: refused=0 with \
                 pinned=0 means the door never ran, refused=0 with pinned>0 means the \
                 screen was inert on this workload)"
            );
            // The two PUBLICATION refusals, on one line for the same reason.
            // `tiered.rs`'s `JIT admission gate:` line already prints
            // `stale-install-epoch-refusals=`; `stale_dependency_refusals()` had
            // ZERO callers workspace-wide, even though `tiered.rs:2671`'s own
            // preamble names it among the counters that dump reports. It does
            // not report it.
            //
            // The two are the same event class with different causes and they
            // are only useful as a pair: `epoch` is "compiled against bytecode a
            // later redefinition replaced", `dependency` is "a later invalidation
            // withdrew a class this body inlined or speculated on". Both cost one
            // wasted compile and both are fail-closed, so neither is a crash —
            // but a workload where `dependency` climbs while `epoch` stays flat
            // is class-unloading or speculation churn, and the reverse is
            // redefinition churn, and a single total cannot say which. Again the
            // companion is reprinted rather than cross-referenced, for the same
            // reason as the line above.
            let pub_dep = cratonvm_jit::stale_dependency_refusals();
            let pub_epoch = cratonvm_jit::stale_install_epoch_refusals();
            eprintln!(
                "[cratonvm] JIT publication refusals: stale-dependency={pub_dep} \
                 stale-install-epoch={pub_epoch} (dependency climbing alone is \
                 invalidation/speculation churn; epoch climbing alone is \
                 redefinition churn)"
            );
            // The code-cache LIFECYCLE report: what was installed, what was
            // withdrawn, what actually came back to the OS, what is still
            // mapped only because reclamation could not be proven safe, and the
            // refusals.
            //
            // Round 10 built this report over three waves and it had no reader.
            // Wave 5 inverted the data flow so that
            // `code_cache_lifecycle_raw()` PULLS from `cratonvm_jit` (the crate
            // graph forbids the push: `cratonvm-vm` depends on `cratonvm-jit`
            // and there is no edge back). Wave 6 deleted the dead push
            // wrappers. Then wave 6 asked the question that work invites — now
            // that the pull exists, who reads it? — and the answer was nobody:
            // every reference to `code_cache_lifecycle_report` in the tree was
            // its own declaration, a doc comment, or
            // `#[cfg(test)] mod tests`. So the JIT's installs, retirements,
            // reclamation, cap, refused allocations and arena free space, every
            // ratio derived from them, and the whole `Display` — including the
            // poison-free warning and the keep-alive census, both of which are
            // must-be-zero correctness signals — ran only under `cargo test`
            // (`docs/feature-designs/real-jdk-mode.md`,
            // Finding 1). This line is the reader.
            //
            // **It could not be added until Finding 2 was fixed, and that
            // ordering is the substance of this change, not a caveat.** Seven
            // `CodeCacheLifecycleRaw` fields had no producer at all, and two of
            // the resulting readings were not merely zero but arithmetically
            // impossible: a `peak` of 0 beside a large `live` (a high-water
            // mark cannot be below the current value), and a
            // `versions_per_method` of `0.0000` beside a positive `installs` (a
            // method with a body has at least one version; `0.0` is what the
            // module's `ratio` helper answers for a zero denominator). Wiring
            // the print first would have shipped both to operators, and a wrong
            // number gets acted on where a missing one gets investigated. The
            // suppression landed first, in the same commit:
            // `CodeCacheLifecycleRaw::modelled_sources` now records which
            // groups a snapshot has a producer for, and the `Display` prints a
            // named `n/a` with the reason for each group that has none. So the
            // four `n/a`s this line can emit are a statement about
            // `cratonvm_jit`'s accounting, not a failure of this call.
            //
            // On `flags().jit.method_stats` — the same switch as every line
            // above — because this is what the compiler did, read at exit,
            // which is the criterion the `loop-xform admission` line at the
            // bottom of this block states. NOT on the arena's own flag like the
            // census further down: the arena contributes only the free-space
            // triple, and installs, retirements, reclamation and refusals are
            // all real with the arena off.
            //
            // Cheap enough to sit on a switch that is already accepted: a few
            // dozen relaxed loads and some float division, plus — only when
            // `CRATONVM_JIT_CODE_ARENA` is on — one arena census, which takes
            // the arena lock and walks its live blocks. This is the last thing
            // the process does, so that lock is uncontended.
            //
            // Printed unconditionally rather than "only when something moved",
            // deliberately, and for the reason the implicit-null health line
            // three screens up gives: an all-zero code cache is itself a
            // reading. `installed=0 bodies` says the JIT never published
            // anything on this workload, which is the first thing to rule out
            // when a perf claim does not reproduce, and it is not distinguish-
            // able from a missing line.
            //
            // A bare `eprintln!` of the `Display`, whose lines carry their own
            // `[JIT]` tag rather than `[cratonvm]`. That is the established
            // idiom for a multi-line subsystem report in this file — see
            // `eprintln!("{}", cratonvm_vm::collector_decision_report())` in the
            // GC block above, whose lines carry `[GC]` — and it is clean under
            // `scripts/check-no-diag-prints.sh`, which forbids the bring-up tags
            // `[WP*]`/`[DIAG*]`/`[TRACE*]`/`[SB-TRACE]`/`[BUFFER]`/`[CV*]`/
            // `[FJP*]`/`[FUT*]` and nothing else. Re-tagging every line would
            // have meant either reformatting the report at this call site or
            // changing a `Display` that several of `code_cache_lifecycle.rs`'s
            // own tests assert substrings of (seven of them at this commit —
            // stated without pinning the number here, since a count in a
            // comment in another crate is a count that rots).
            eprintln!(
                "{}",
                cratonvm_vm::jit::code_cache_lifecycle::code_cache_lifecycle_report()
            );
        }
        // Reference loads whose slot did NOT hold a reference, contained by
        // `GETFIELD_EXPECT_REFERENCE` instead of being handed to compiled code
        // as a pointer. Printed even when zero, and on the same switch: this
        // number is the difference between "the crash stopped happening" and
        // "the crash stopped being reachable this hour" -- the workload it was
        // found on crashed 3 times in 38 runs one hour and 0 in 129 the next.
        //
        // Non-zero is NOT good news. It counts type-punned reference slots this
        // VM is still producing (the G30-1 species); the guard only stops them
        // becoming wild pointers in compiled code.
        eprintln!(
            "[cratonvm] getfield reference loads that contained a primitive slot: {} \
             (of which the payload word was NON-ZERO, i.e. would have been \
             dereferenced: {})",
            cratonvm_vm::jit::helpers::jit_getfield_primitive_in_ref_slot(),
            cratonvm_vm::jit::helpers::jit_getfield_punned_ref_nonzero()
        );
        // The WRITE side of the same species, and the two ENGAGEMENT counters
        // for the substitutions that could produce it. All three printed even
        // when zero, for the reason the counter above them is:
        //
        // * `unresolved field sites refused` is how often a backend declined a
        //   `getfield`/`putfield`/`getstatic`/`putstatic` because it had no
        //   resolved slot. Those four sites used to substitute slot 0 tagged
        //   `int` and carry on, which is how JDT's `HashtableOfInt.rehash`
        //   wrote an `int[]` into slot 0 as `Value::Int(low32_of_ptr)`. A zero
        //   here says the substitution never fired on this workload -- which is
        //   the observation that has to precede blaming it for anything.
        // * `field sites refused for a descriptor disagreement` is how often
        //   the name-only field resolver found a field whose descriptor is not
        //   the one the constant pool names, so the site's slot index and its
        //   type tag would have described different fields.
        // * `compiled primitive stores into a declared-reference slot` counts
        //   only while `CRATONVM_DBG_WATCH_PUN` is armed; it is `<not armed>`
        //   otherwise rather than `0`, because those are different facts.
        eprintln!(
            "[cratonvm] unresolved field sites refused: {} | field sites refused \
             for a descriptor disagreement: {} | compiled primitive stores into \
             a declared-reference slot: {}",
            cratonvm_jit::x64::bytecode_walk::unresolved_field_site_bails(),
            cratonvm_vm::runtime::interpreter::jit_field_tag_disagreements(),
            cratonvm_vm::jit::helpers::jit_putfield_primitive_into_ref_slot()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<not armed>".to_string()),
        );
        // Compiled field stores the helper family DISCARDED. Both drops are
        // right in isolation -- writing through an implausible receiver, or
        // past the end of the object, corrupts the neighbouring allocation
        // instead. What they lacked is visibility: a dropped store leaves the
        // field exactly as it was, and for a field assigned once in a
        // constructor that is the zero-filled cell a reference `getfield` reads
        // back as an ordinary `null`. The defect then surfaces arbitrarily far
        // away as "this reference cannot be null", with nothing connecting it
        // to a store that did not happen.
        //
        // Printed even when zero, because zero is the useful reading: it rules
        // the whole mechanism out for a run, which is what a null-reference
        // investigation needs before it starts looking anywhere else.
        // `CRATONVM_DBG_DROPPED_PUTFIELD=1` names the receiver, the slot and
        // the compiled method for the first 32 of each.
        let (dropped_recv, dropped_oob) = cratonvm_vm::jit::helpers::jit_putfield_dropped_stores();
        // ENGAGEMENT counter for the JVMS 5.4.6 reclassification: how many
        // compiled call sites were bound directly because their constant pool
        // resolved to a PRIVATE method, instead of being dispatched from the
        // receiver's class. Without it, "the workload passes now" cannot be
        // told from "no site on this workload was one" -- and the shape (a
        // constructor calling its own `private void init()`, subclassed) is
        // common enough that a zero on a large workload is itself a finding.
        eprintln!(
            "[cratonvm] invokevirtual sites pinned to a private target: {}",
            cratonvm_jit::private_invokevirtual_pinned()
        );
        // Fieldrefs where a field of the NAME exists but not with the
        // constant pool's descriptor. Since 2026-08-28 each of these raises
        // NoSuchFieldError, which is what JVMS 5.4.3.2 says; before it, each
        // was answered by that other field.
        //
        // Deliberately NOT counting the case where the name exists nowhere:
        // that is NoSuchFieldError either way, so counting it would report
        // behaviour changes that did not happen -- as it did on SLF4J's own
        // version sanity check, which probes for a field it expects to be
        // absent and catches the error by design.
        //
        // A non-zero here on a workload that used to pass is the first place
        // to look; `CRATONVM_DBG_FIELD_DESCRIPTOR=1` names each one, and
        // `CRATONVM_FIELD_RESOLUTION_NAME_ONLY=1` restores the old answer for
        // a single-binary A/B.
        eprintln!(
            "[cratonvm] fieldrefs whose name exists but not with the recorded descriptor (now NoSuchFieldError): {}",
            cratonvm_vm::runtime::resolve::field_resolution_descriptor_fallbacks()
        );
        // And the number that says whether applying the descriptor CHANGED an
        // answer. Every one of these is a field access that used to reach a
        // same-named field of another type -- and in compiled code, to pair
        // that field's slot index with the constant pool's type tag.
        eprintln!(
            "[cratonvm] field resolutions the descriptor key answered differently from the name-only key: {}",
            cratonvm_vm::runtime::resolve::field_resolution_descriptor_corrections()
        );
        eprintln!(
            "[cratonvm] compiled field stores dropped: implausible receiver={dropped_recv} slot out of bounds={dropped_oob}"
        );
        // G1 parallel-evacuation CAS losses, i.e. how often a worker found
        // another worker had already forwarded the object it was copying and
        // adopted that target. Until 2026-08-26 that arm returned the target
        // WITHOUT recording `old -> target` in this cycle's `pointer_map`, so
        // no root naming `old` could be remapped and it dangled once the
        // region was reused -- the rare `java/lang/Object`.
        //
        // Printed even when zero, for the reason the counter above it is: a
        // fix to an arm nothing reaches is a no-op dressed as a repair, and
        // this arm needs TWO WORKERS RACING ON ONE OBJECT, so a quiet run
        // means the race did not happen, not that the fix is inert.
        eprintln!(
            "[cratonvm] G1 evacuation CAS losses (forwards this VM would have \
             dropped before the 2026-08-26 fix): {}",
            cratonvm_vm::g1_evacuate_cas_loser_forwards()
        );
        // A peer's forwarding claim met still BUSY and waited out. Before
        // 2026-09-30 a wait past the bound abandoned the claim and left the
        // slot on the from-space copy (g1-parallel-evac-pool-exhaustion-loses-
        // a-held-monitor); a zero here means the window was not reached.
        eprintln!(
            "[cratonvm] G1 evacuation claim waits (abandoned before the 2026-09-30 fix when longer than the bound): {}",
            cratonvm_vm::g1_parallel_evac_claim_waits()
        );
        // The `validate_code_ptr` memo's engagement, on the same switch and for
        // the same reason as every counter above it. The memo replaced a global
        // `Mutex` taken on EVERY compiled call; a run where `hits` is 0 has the
        // lock back and would still time within noise of one where it is not.
        {
            let (hits, misses) = cratonvm_jit::code_ptr_memo_stats();
            eprintln!(
                "[cratonvm] code-ptr memo: hits={hits} misses={misses} enabled={} \
                 region_epoch={}",
                cratonvm_jit::code_ptr_memo_enabled(),
                cratonvm_jit::code_ptr_regions_epoch()
            );
        }
        // JIT-side only: these are the sites `helpers.rs` tags by hand. The
        // whole-VM per-caller census that used to print beneath this was
        // retired once it had answered — it cost 3.4x on ZGC, which is how
        // the getfield page's first round of numbers came out wrong.
        eprintln!(
            "[cratonvm] membership walks by JIT site: {}",
            cratonvm_vm::jit::helpers::membership_walks_by_site()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // What the SPLICED-CALL emitter actually emitted, per arm. Zeros are
        // printed: a feature measuring "no different from the arm below it" and
        // a feature that never fired look identical in a timing table, and this
        // is the only line that separates them.
        eprintln!(
            "[cratonvm] inline call arms: {}",
            cratonvm_jit::metrics::inline_call_arm_emits()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // Compiled local exception handlers. `sites-emitted` counts STUBS, not
        // catches; `entered` is the only number that says a `catch` block ran
        // in compiled code, and `propagated` is its correct-but-not-a-win
        // sibling. Zeros printed, for the same reason as the line above.
        eprintln!(
            "[cratonvm] local handlers: {}",
            cratonvm_jit::metrics::local_handler_counts()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // Receiver-type speculation and the per-bci de-spec registry it now
        // consults. `sites-declined` is the only number that says the consult
        // engaged; `guards-emitted` separates "wired and never needed" from
        // "no guarded site compiled at all"; `escalations-spared` is the policy
        // half -- whole-method blacklists withheld from an already-withdrawn
        // speculation. Zeros printed, for the same reason as the lines above.
        eprintln!(
            "[cratonvm] receiver despec: {} escalations-spared={}",
            cratonvm_jit::metrics::receiver_despec_counts()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" "),
            cratonvm_jit::metrics::despec_escalations_spared()
        );
        // How WIDE each surviving blind spill was. `stores-emitted` against
        // `stores-if-full` is the narrowing's engagement AND its size in one
        // ratio; `full-refused` separates "narrowing kept everything" from
        // "narrowing was off".
        eprintln!(
            "[cratonvm] safepoint spill width: {} args-published={}",
            cratonvm_jit::metrics::spill_width_counts()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" "),
            cratonvm_jit::metrics::spill_args_published_count()
        );
        // The per-call safepoint blind spill, and how often the oop-clean-frame
        // proof let a direct call publish the safepoint id alone instead of
        // copying the whole GPR file into the frame. `elided` is the engagement
        // counter; the two refusal rows say why the others did not.
        eprintln!(
            "[cratonvm] direct-call spill: {}",
            cratonvm_jit::metrics::call_spill_counts()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // The RECEIVER-SHAPE census: which guard clause each helper call
        // actually failed, counted at EXECUTION on the one path every
        // fall-through crosses. The two lines above count EMISSIONS, which is
        // the question that cost this page four dead hypotheses. Needs
        // `CRATONVM_DBG_GETFIELD_RECEIVERS=1`; all-zero means it was not on.
        eprintln!(
            "[cratonvm] getfield receiver shapes: {}",
            cratonvm_vm::jit::helpers::jit_getfield_receiver_shapes()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // What moved the epoch every compiled field site guards on: a moved
        // epoch sends compact, in-bounds receivers (`compact-eligible` above)
        // to the helper. Unloads no longer move it.
        let (replaced, unloads_spared) = cratonvm_types::layout_epoch_census();
        eprintln!(
            "[cratonvm] field-site layout epoch: epoch={} replacements={replaced} unloads-spared={unloads_spared}",
            cratonvm_types::layout_replace_epoch(),
        );
        eprintln!(
            "[cratonvm] getfield out-of-bounds by field kind: {}",
            cratonvm_vm::jit::helpers::jit_getfield_oob_field_kinds()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // The ALLOCATION side of the same question, from the path
        // `plan_object_alloc`'s `[compact-legacy]` census cannot see: the TLAB
        // fast path writes a legacy header unconditionally and serves ~99% of
        // allocations, so a class can be 100% of the legacy field receivers
        // above and appear in no allocation census at all. Needs
        // `CRATONVM_DBG_COMPACT_LEGACY`.
        let tlab_legacy = cratonvm_vm::runtime::interpreter::tlab_legacy_object_classes();
        if !tlab_legacy.is_empty() {
            eprintln!(
                "[cratonvm] TLAB-allocated legacy objects by class: {}",
                tlab_legacy
                    .iter()
                    .map(|(n, id, c)| format!("{n}(id={id})={c}"))
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
        let legacy_classes = cratonvm_vm::jit::helpers::jit_getfield_legacy_receiver_classes();
        if !legacy_classes.is_empty() {
            eprintln!(
                "[cratonvm] getfield legacy receivers by class: {}",
                legacy_classes
                    .iter()
                    .map(|(n, id, c)| format!("{n}(id={id})={c}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        let eligible = cratonvm_vm::jit::helpers::jit_getfield_compact_eligible_receivers();
        if !eligible.is_empty() {
            eprintln!(
                "[cratonvm] getfield compact-eligible receivers by field: {}",
                eligible
                    .iter()
                    .map(|(n, c)| format!("{n}={c}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        eprintln!(
            "[cratonvm] IR-tier inline-getfield refusals: {}",
            cratonvm_jit::metrics::ir_getfield_declines()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // The bytecode loop rewriter's admission tally, on the same switch and
        // for the same reason: it is what the compiler did, read at exit. The
        // counters themselves are always collected (they do not consult
        // `metrics::enabled()`), so this prints real numbers from a default
        // run — which is the measurement that retired three of the four gates
        // (`loop-02-planner-admission-gates.md`) and is
        // what would say immediately if one of them got back in the way.
        //
        // The four condition rows OVERLAP: a method with an `invokedynamic`
        // compiled under `deopt_real` is in both. Read each against
        // `loop_xform_compiles`; never sum them. Only `loop_xform_inline_sites`
        // still refuses; the other three are counted and admitted.
        let tally = cratonvm_jit::metrics::loop_xform_counts();
        let row = tally
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("[cratonvm] loop-xform admission: {row}");
    }

    // The shared JIT code arena's census, at exit, when the arena is on.
    //
    // This is the call `REVIEW-NOTE 3: measure before flipping the default` at
    // the bottom of `jit/src/platform.rs` already asserted existed. That note
    // lists the evidence a default flip of `CRATONVM_JIT_CODE_ARENA` needs —
    // `served_from_free_list`, `bytes_padding_live`, `served_standalone`,
    // `regions_reclaimed`, `bytes_reclaimed`, `blocks_split` against
    // `served_from_free_list`, and `bytes_free_listed / free_blocks` over time —
    // and then said "`platform::log_jit_code_arena_census` emits all of it at
    // `info!`". It did not: `rg -nw 'log_jit_code_arena_census'` found the
    // definition, that one sentence, and a line of `docs/`, and no call. Anyone
    // following the note would have set `RUST_LOG=info`, run the workload, found
    // no `arena_id=` event and had to write this line before measuring anything
    // (`docs/feature-designs/real-jdk-mode.md`).
    //
    // GATED ON THE ARENA'S OWN FLAG, not on `flags().jit.method_stats`, and the
    // distinction is deliberate. `jit_code_arena_enabled()` resolves
    // `CRATONVM_JIT_CODE_ARENA` once through a `OnceLock`, so a
    // default-configuration run pays one cached bool and emits nothing — and
    // with the arena off every field would be structurally zero anyway, since
    // `ExecutableBuffer::new_in` delegates straight to `new` and no block is
    // ever carved. Putting it behind the method-stats switch instead would have
    // made REVIEW-NOTE 3's protocol need TWO switches to produce one census,
    // which is the kind of drift that makes a written protocol stop being
    // followed.
    //
    // One consequence worth stating, because it is a silent absence rather than
    // a wrong number: `jit_code_arena_enabled()` returns `false` unconditionally
    // when `JIT_CODE_ARENA_IS_INERT`, which is `cfg!(all(target_os = "macos",
    // target_arch = "aarch64"))`. On that target this line emits NOTHING even
    // with `CRATONVM_JIT_CODE_ARENA=1 RUST_LOG=info` set, and that is correct —
    // an inert arena answers every request with a standalone mapping, so there
    // is no pooling to measure — but a reader following REVIEW-NOTE 3 on an
    // Apple Silicon box would otherwise read the missing line as this call
    // having gone missing again. The note asks for the measurement "on Windows"
    // for the same reason.
    //
    // `tracing::info!` rather than an `eprintln!` because the logger already
    // chose that, for a reason worth not re-litigating here: the workspace
    // `Cargo.toml` pins `tracing` with `release_max_level_info`, so `debug!` and
    // `trace!` expand to no-ops in a release build and cannot be recovered with
    // any `RUST_LOG` value — which would make the census unreadable in exactly
    // the runs that would justify turning the arena on. A reader therefore needs
    // `CRATONVM_JIT_CODE_ARENA=1 RUST_LOG=info`, which is what REVIEW-NOTE 3
    // already told them to set.
    //
    // Two calls, not one: `jit_code_arena_census()` takes the arena lock and
    // copies out a `JitCodeArenaCensus` by value, and
    // `log_jit_code_arena_census` formats that snapshot and takes no lock. So
    // the lock is held only for the copy, and none of the ~24 `tracing` field
    // evaluations happen under it. This is the last thing the process does, so
    // the lock is uncontended in practice either way; the ordering is stated
    // because it is a property of the two functions' contracts rather than of
    // this call site, and a future reader inlining them would lose it.
    if cratonvm_jit::platform::jit_code_arena_enabled() {
        cratonvm_jit::platform::log_jit_code_arena_census(
            &cratonvm_jit::platform::jit_code_arena_census(),
        );
    }

    // Phase accounting. Delegate the compile-phase breakdown rather than
    // re-timing it (`docs/observability/phase-accounting.md` §1):
    // `MetricsSummary::phase_totals_ns` is already `Vec<(&'static str, u64)>`,
    // which is exactly `set_compilation_breakdown`'s argument shape, so the
    // report carries `jit::metrics`' own numbers instead of a second set of
    // timers that could disagree with them.
    //
    // The `enabled()` test is not redundant with `emit_configured_sinks`
    // returning `None`: it also keeps a run with accounting off from paying for
    // the `jit::metrics` ring walk that `summary()` performs.
    if phase::enabled() {
        let summary = cratonvm_jit::metrics::summary();
        phase::set_compilation_breakdown(&summary.phase_totals_ns);
        if let Some(outcome) = phase::emit_configured_sinks() {
            // One integer-only line, in the shape
            // `regression-suite/perf/run-cratonbench-gate.sh` already scrapes.
            eprintln!("{}", outcome.report.summary_line());
            // A sink whose path cannot be opened is reported, never raised: a
            // diagnostic that failed a JVM shutdown because a directory is
            // read-only would be a worse defect than the missing artefact.
            if let Some((path, Err(e))) = &outcome.json {
                eprintln!("[cratonvm] phase-accounting JSON sink {path:?} failed: {e}");
            }
            if let Some((path, Err(e))) = &outcome.jfr {
                eprintln!("[cratonvm] phase-accounting JFR sink {path:?} failed: {e}");
            }
        }
    }
}

/// The `--help` long form: the short `about` (the doc comment on [`Args`])
/// plus a summary of the seven mode / Real-JDK diagnostic flags.
///
/// Spelled as a `const` rather than extra doc-comment lines because clap
/// reflows a derived doc comment into paragraphs, which would destroy the
/// alignment of the table below. The wording is contract §9 of
/// `docs/book/src/user-guide/real-jdk-mode.md`; keep the two in step.
pub(crate) const LONG_ABOUT: &str = "\
CratonVM - A Java Virtual Machine implemented in Rust.

Executes Java programs by loading and interpreting `.class` files.

Usage: cratonvm [OPTIONS] <CLASS_NAME> [ARGS]...
       cratonvm [OPTIONS] --jar <FILE.jar> [ARGS]...

Help: --cratonvm-help prints this text. -help, -h, -?, --help, -X and
--help-extra print the JDK 25 java launcher's own help texts, as HotSpot
does (CRATONVM_LAUNCHER_HOTSPOT_HELP_TEXT=0 gives them back to this text).

Class library and compatibility policy:
  (no flag)                     THE DEFAULT: real JDK class
                                library, and real class bytes are authoritative
                                -- no fabricated compatibility class and no
                                synthetic-stub native. Same as --real-jdk.
  --real-jdk                    States the default explicitly. Kept so scripts,
                                CI lanes and bug reproductions can name the
                                policy they ran under. Conflicts with
                                --fast-boot, --compatible and --synthetic-jdk.
  --fast-boot, --compatible     Real JDK class library with fast-boot optimizations
                                tailored for sub-second cold starts, serverless, and
                                rapid integration testing. Enables deferred verification
                                maps, lazy core-class bootstrap, lightweight boot layer,
                                and JIT warmup delay.
  --synthetic-jdk               Standalone synthetic class library (~5,200 Rust
                                stubs). Implies --fast-boot (the synthetic
                                library IS the substitutions strict mode
                                forbids). Conflicts with --real-jdk/--fast-boot
                                as a LIBRARY choice, and with --real-jdk as a
                                POLICY choice. Needs a binary built with the
                                `synthetic-jdk` Cargo feature, which is NOT in
                                the default set; without it the launcher exits
                                with an error instead of starting a VM with no
                                class library. `-Xinternalversion` reports
                                whether this build has it
                                (jdk.mode.synthetic_compiled_in).

Real-JDK diagnostics (all four work in either compatibility mode, so a
--compatible run can be censused for what strict mode would reject):
  --real-jdk-report <FILE>      Write the JSON violation/counter report.
  --dump-class-origins <FILE>   Write the class-origin census (one row per
                                loaded class, with its provenance).
  --trace-real-jdk              Log every recorded violation to stderr.
  --explain-real-jdk            Print the long-form explanation for each
                                violation, and leave absolute paths unredacted
                                in the reports.

Differential mode (docs/testing/diff-hotspot.md):
  --diff-hotspot                Run this same program under CratonVM and under a
                                reference JDK and report the FIRST divergence in
                                stdout / stderr / exit status. Exit 0 identical,
                                1 diverged, 2 no usable reference JDK, 3 the
                                CratonVM side was not self-consistent.
  --diff-ignore <PATTERN>       Mask any output line containing PATTERN on both
                                sides ('*' is a wildcard). Repeatable.
  --diff-runs <N>               CratonVM-side runs used to detect the program's
                                own nondeterminism before blaming HotSpot
                                (default 2; 1 disables the check).
  --diff-timeout <SECONDS>      Per-child wall clock (default 120).
  --diff-java-arg <ARG>         Extra argument for the reference `java` only.
                                Repeatable.
  --diff-strict                 Report a difference that only the built-in
                                nondeterminism maskers explain as a failure.
";

/// CratonVM - A Java Virtual Machine implemented in Rust.
///
/// Executes Java programs by loading and interpreting `.class` files.
///
/// Usage: cratonvm [OPTIONS] <CLASS_NAME> [ARGS]...
///        cratonvm [OPTIONS] --jar <FILE.jar> [ARGS]...
#[derive(Parser, Debug)]
#[command(name = "cratonvm", version, about, long_about = LONG_ABOUT)]
pub(crate) struct Args {
    /// The fully qualified class name to execute (e.g., com.example.Main).
    pub(crate) class_name: Option<String>,

    /// Execute a JAR file. The main class is read from META-INF/MANIFEST.MF.
    /// When -jar is used, the -cp/-classpath flag is ignored; the classpath
    /// comes from the JAR itself and its manifest Class-Path attribute.
    #[arg(long = "jar", value_name = "FILE")]
    pub(crate) jar: Option<String>,

    /// Source version for single-file or multi-file source-code programs (mirrors JDK `--source`).
    #[arg(long = "source", value_name = "N", overrides_with = "source")]
    pub(crate) source: Option<String>,

    // Classpath: directories and JAR files to search for classes.
    // `overrides_with` (self) makes a repeated flag last-wins instead of a hard
    // error, matching the real `java` launcher. Maven Surefire/the WildFly
    // testsuite fork with `-Xmx512m` twice (surefire memory args + jvm.args);
    // without this clap aborts with "cannot be used multiple times" (exit 2),
    // which Surefire reports as "forked VM terminated without saying goodbye".
    #[arg(
        short = 'c',
        long = "classpath",
        alias = "cp",
        overrides_with = "classpath"
    )]
    pub(crate) classpath: Option<String>,

    /// Maximum heap size (e.g., 256m, 1g).
    #[arg(long = "Xmx", value_name = "SIZE", overrides_with = "max_heap")]
    pub(crate) max_heap: Option<String>,

    /// Initial heap size (e.g., 16m, 512m).
    ///
    /// F-16: this used to be accepted and discarded, because the heap was
    /// allocated at `-Xmx` in the collector's constructor and there was nothing
    /// for an initial size to mean. Every backend now RESERVES `-Xmx` and
    /// COMMITS on demand, so `-Xms` names the prefix committed up front.
    ///
    /// Honoured on all three collectors as of 2026-09-21; G1 got it first
    /// (F-16) and the other two followed. An `-Xms` above `-Xmx` is clamped
    /// rather than refused. On Generational the value commands the young pair,
    /// whose old-generation sibling is committed in full at construction
    /// whatever `-Xms` says, so the startup commit is at least `-Xmx / 2` there
    /// in any case.
    #[arg(long = "Xms", value_name = "SIZE", overrides_with = "initial_heap")]
    pub(crate) initial_heap: Option<String>,

    /// Young generation size (`-Xmn<size>`), e.g. 256m.
    ///
    /// gen r4w2/alloc2 (2026-09-23): honoured by the Generational collector
    /// (`-XX:+UseGenerationalGC`), where it sets the from/to semi-space pair
    /// (each semi-space gets half); ignored with a one-line note on G1 and
    /// ZGC. Until this date the flag was swallowed by the `-X…` catch-all.
    #[arg(long = "Xmn", value_name = "SIZE", overrides_with = "young_xmn")]
    pub(crate) young_xmn: Option<String>,

    /// Default thread stack size (`-Xss<size>`), e.g. 64m.
    ///
    /// gcd d6/f (2026-09-28): the stack a Java platform thread gets when its
    /// `Thread` was constructed without a `stackSize` (`VmConfig::default_thread_stack_size`),
    /// as on HotSpot. A size the VM's 8 MiB carrier default already covers
    /// changes nothing; a larger one grows the carrier. Until this date the
    /// flag was swallowed by the `-X…` catch-all.
    #[arg(
        long = "Xss",
        value_name = "SIZE",
        overrides_with = "thread_stack_size"
    )]
    pub(crate) thread_stack_size: Option<String>,

    /// `-XX:NewSize=<size>` → floor on the young generation (Generational
    /// only; see `cratonvm_gc::gen_young_sizing`).
    #[arg(
        long = "XX:NewSize",
        value_name = "SIZE",
        overrides_with = "young_new_size"
    )]
    pub(crate) young_new_size: Option<String>,

    /// `-XX:MaxNewSize=<size>` → ceiling on the young generation (Generational
    /// only).
    #[arg(
        long = "XX:MaxNewSize",
        value_name = "SIZE",
        overrides_with = "young_max_new_size"
    )]
    pub(crate) young_max_new_size: Option<String>,

    /// `-XX:NewRatio=<n>` → young = heap / (n + 1) (Generational only).
    #[arg(
        long = "XX:NewRatio",
        value_name = "N",
        overrides_with = "young_new_ratio"
    )]
    pub(crate) young_new_ratio: Option<String>,

    /// Print verbose class loading information.
    #[arg(long = "verbose:class")]
    pub(crate) verbose_class: bool,

    /// Print verbose GC information.
    #[arg(long = "verbose:gc")]
    pub(crate) verbose_gc: bool,

    /// Boot classpath (overrides JAVA_HOME auto-discovery).
    #[arg(long = "Xbootclasspath", value_name = "PATH")]
    pub(crate) boot_classpath: Option<String>,

    /// Append to the bootstrap class search AFTER the runtime image (HotSpot
    /// `-Xbootclasspath/a:PATH`; repeatable, searched in order).
    #[arg(long = "Xbootclasspath-append", value_name = "PATH")]
    pub(crate) boot_classpath_append: Vec<String>,

    /// HotSpot `-Xbootclasspath/p:PATH`, which JDK 9+ refuses at startup.
    #[arg(long = "Xbootclasspath-prepend", value_name = "PATH", hide = true)]
    pub(crate) boot_classpath_prepend: Option<String>,

    /// JAVA_HOME path for automatic boot/ext classpath discovery.
    #[arg(long = "java-home", value_name = "PATH")]
    pub(crate) java_home: Option<String>,

    /// Skip bytecode verification (like -noverify / -Xverify:none).
    #[arg(long = "noverify")]
    pub(crate) noverify: bool,

    /// Disable JIT compilation (interpreter-only execution).
    ///
    /// Equivalent to setting `CRATONVM_DISABLE_JIT=1` in the environment.
    /// Useful for diagnosing whether a misbehaviour originates in the JIT
    /// versus the interpreter, and as a safety fallback when the JIT is
    /// known to mis-compile a particular library.
    #[arg(long = "nojit")]
    pub(crate) nojit: bool,

    /// Bytecode verification policy (-Xverify:none|remote|all).
    /// `none` skips verification entirely (equivalent to --noverify).
    /// `remote` (HotSpot default) verifies non-boot classes only.
    /// `all` verifies boot classes too.
    #[arg(long = "Xverify", value_name = "MODE")]
    pub(crate) xverify: Option<String>,

    /// CDS shared archive file path (-XX:SharedArchiveFile=path).
    #[arg(long = "XX:SharedArchiveFile", value_name = "PATH")]
    pub(crate) shared_archive_file: Option<String>,

    /// CDS sharing mode (-Xshare:off/on/auto/dump).
    #[arg(long = "Xshare", value_name = "MODE", default_value = "off")]
    pub(crate) xshare: String,

    /// Select the synthetic class library (~5,200 Rust stubs in
    /// `native-builtins`) instead of real JDK bytecode.
    ///
    /// Mutually exclusive with `--compatible` / `--real-jdk`. Requires a
    /// build with the `synthetic-jdk` Cargo feature; without it the launcher
    /// errors out rather than booting a VM with no class library (see
    /// `cratonvm_vm::config::require_synthetic_jdk`).
    ///
    /// **Implies `--compatible`.** The synthetic class library IS ~5,200
    /// synthetic-stub natives, which is exactly the set the default strict
    /// policy forbids, so `--synthetic-jdk` under the default would select a
    /// VM with no usable class library. There is no strict synthetic mode by
    /// construction, so the policy is implied rather than refused — but
    /// spelling `--real-jdk --synthetic-jdk` explicitly is still an error,
    /// because that argv states a contradiction instead of omitting one.
    ///
    /// The launcher's JDK-mode default is real
    /// (`cratonvm_vm::config::LAUNCHER_DEFAULT_JDK_MODE`), fixed at
    /// compile time. It is NOT derived from whether a JDK happens to be
    /// installed — see the determinism note on
    /// `cratonvm_vm::config::JdkMode`.
    #[arg(long = "synthetic-jdk", conflicts_with_all = ["real_jdk", "fast_boot"])]
    pub(crate) synthetic_jdk: bool,

    /// Fast-boot compatible mode: real JDK class library optimized for
    /// sub-second cold starts, fast integration testing, and serverless execution.
    ///
    /// Enables deferred verification maps, lazy core-class bootstrap,
    /// instant boot layer initialization, and JIT warmup throttling.
    /// Aliased by `--compatible`.
    ///
    /// If no usable JDK is found the launcher fails with a message naming
    /// everything it searched — it never silently substitutes the synthetic
    /// library.
    #[arg(long = "fast-boot", visible_alias = "compatible", conflicts_with = "real_jdk")]
    pub(crate) fast_boot: bool,

    /// Strict compatibility policy: real JDK class bytes are authoritative.
    ///
    /// Requires a real runtime image (there is no silent fallback) and
    /// additionally forbids the two substitutions the fast-boot mode allows:
    /// no fabricated compatibility class (`ClassOrigin::CompatibilityStub`) may be
    /// minted, and no `NativeKind::SyntheticStub` may be registered. Reviewed
    /// bridges and intrinsics, arrays, hidden classes, lambdas, proxies and
    /// reflection accessors all remain legal — they are products of a conforming JVM,
    /// not compatibility substitutions.
    ///
    /// Conflicts with `--synthetic-jdk`: the synthetic library IS the set of
    /// substitutions this flag forbids, so the pair selects a VM with no
    /// usable class library.
    ///
    /// **This is the launcher default.** The flag is
    /// retained so the policy can be stated explicitly (in scripts, CI lanes
    /// and bug reproductions), so every existing `--real-jdk` invocation
    /// keeps working unchanged, and so `--real-jdk --fast-boot` is a
    /// diagnosable contradiction rather than a last-flag-wins coin toss.
    ///
    /// Wave 1 is measurement-first: violations that cannot yet be enforced
    /// safely are recorded and counted rather than fatal. Pair with
    /// `--real-jdk-report` to see them. See
    /// docs/book/src/user-guide/real-jdk-mode.md.
    #[arg(long = "real-jdk", alias = "jdk-only", conflicts_with = "synthetic_jdk")]
    pub(crate) real_jdk: bool,

    /// Write the Real-JDK violation/counter report to the given JSON file.
    ///
    /// Schema (`schema_version` 1): `{ mode, jdk_feature, violations[],
    /// counts{}, refusals{}, observation_sink{}, census_exemptions{},
    /// enforcement_dial{} }`, where `counts` carries the four class-origin
    /// buckets and the three per-`NativeKind` invocation totals, and
    /// `refusals` carries the four exact refusal-event tallies that have no
    /// place in the closed `counts` set. Violations are sorted so the file is
    /// diff-stable, and absolute paths are redacted unless
    /// `--explain-real-jdk` is also passed.
    ///
    /// **The envelope grows ADDITIVELY and `schema_version` stays 1.** The
    /// three sibling objects are what qualify `violations[]` rather than join
    /// it, and each was added later than the version number:
    /// `observation_sink` says whether the array is the population or a floor
    /// *for want of room*; `census_exemptions` says how much of §1.4 the array
    /// cannot contain *by construction*, because `NativeKind::Intrinsic` is the
    /// reviewed exception and no recorder fires for it; `enforcement_dial`
    /// says whether `CRATONVM_ENFORCE_NATIVE_SHADOW` was armed, which REMOVES
    /// rows. A consumer that indexes by key is unaffected by any of them.
    ///
    /// Two violation kinds — `compatibility-class-requested` and
    /// `native-shadows-bytecode` — also carry `class_origin`. It is absent on
    /// the other five on purpose: they fire at REGISTRATION, before any class
    /// is loaded, so the column would be a `null` that ticks §1.7's box without
    /// answering it.
    ///
    /// `violations[]` unions all five recording sites: refused native
    /// registrations, compatibility-class requests, JIT compile-time direct
    /// binds, JIT fast-path admissions and interpreter bytecode-wins
    /// observations. A source that is silently omitted would make an empty
    /// array read as "clean" when it means "not measured", so the fold is
    /// all-or-nothing.
    ///
    /// Works in either compatibility mode. Under `--compatible`
    /// the report is a census of what strict mode *would* reject, built from
    /// the class-origin side alone: the four other sites only record when
    /// strict policy actually refuses something, so they and every `refusals`
    /// tally are zero there.
    #[arg(
        long = "real-jdk-report",
        alias = "jdk-only-report",
        value_name = "FILE"
    )]
    pub(crate) jdk_only_report: Option<String>,

    /// Write the class-origin census to the given JSON file.
    ///
    /// Schema: `{ schema_version, counts{<origin-tag>: n, total},
    /// classes: [{name, origin, reason, requested_by, real_bytes_found,
    /// loader_id}] }`, one row per class the class manager holds, sorted by
    /// `(name, loader_id, origin)`. The `origin` tags are the stable
    /// `ClassOrigin::as_str()` spellings (`boot-image`, `vm-array`,
    /// `generated-lambda`, `compatibility-stub`, ...).
    #[arg(long = "dump-class-origins", value_name = "FILE")]
    pub(crate) dump_class_origins: Option<String>,

    /// Log each Real-JDK violation to stderr as it is picked up.
    ///
    /// All five violation logs are drained once immediately after VM init
    /// (which is when registration refusals happen) and again at shutdown, on
    /// the same schedule, so the trace and `--real-jdk-report` name the same
    /// set. Each drain also prints one refusal-counter delta line when any
    /// counter moved — that is the only way `jit_inline_cache_natives`, which
    /// has no violation object behind it, becomes visible in a traced run.
    ///
    /// Compile-time and dispatch-time refusals cannot have happened yet at the
    /// post-init drain, so in practice they all surface at shutdown.
    #[arg(long = "trace-real-jdk", alias = "trace-jdk-only")]
    pub(crate) trace_jdk_only: bool,

    /// Print the long-form, operator-facing explanation for each Real-JDK
    /// violation instead of the one-line summary, and leave absolute paths
    /// unredacted in the reports and census files.
    #[arg(long = "explain-real-jdk", alias = "explain-jdk-only")]
    pub(crate) explain_jdk_only: bool,

    /// Enable Panama FFI native access (mirrors JDK `--enable-native-access`).
    ///
    /// Grant native access to the named modules or ALL-UNNAMED. Restricted
    /// downcall linking without a grant follows --illegal-native-access.
    ///
    /// The optional value (`ALL-UNNAMED` or a module name) mirrors the JDK
    /// spelling and scopes the grant to those modules. The flag is also
    /// accepted with no value at all (bare `--enable-native-access`). Optional
    /// values must use `=` so a following main class is not consumed as the
    /// module name. Because clap accepts the long name directly, the JDK
    /// invocation (`--enable-native-access=ALL-UNNAMED`) passes through
    /// unchanged. zt t16/tg7: the JDK's separate form
    /// (`--enable-native-access ALL-UNNAMED`) is joined into the `=` form
    /// before clap (`join_enable_native_access_space_form`,
    /// `CRATONVM_LAUNCHER_NATIVE_ACCESS_SPACE_FORM`), as libjli takes it; a
    /// bare flag with no value is then the JDK launcher's missing-value
    /// refusal.
    #[arg(
        long = "enable-native-access",
        value_name = "MODULE",
        num_args = 0..=1,
        default_missing_value = "ALL-UNNAMED",
        require_equals = true,
    )]
    pub(crate) enable_native_access: Option<String>,

    /// What to do when a RESTRICTED method is called without a native-access
    /// grant (mirrors JDK `--illegal-native-access=allow|warn|deny`).
    ///
    /// Default `warn`, which is what a JDK 25 launcher does — measured on
    /// Adoptium 25.0.4, `MemorySegment.reinterpret` succeeds with four WARNING
    /// lines and throws only under `deny`. CratonVM used to deny
    /// unconditionally with no way to ask for anything else, which is why this
    /// flag arrives together with the default change: the strict behaviour is
    /// still available, it is just no longer the only behaviour.
    #[arg(
        long = "illegal-native-access",
        value_name = "MODE",
        default_value = "warn"
    )]
    pub(crate) illegal_native_access: String,

    /// Enable preview features (mirrors JDK `--enable-preview`).
    ///
    /// A class file whose `minor_version` is 65535 at the running JVM's major
    /// version is a preview class file (JVMS 4.1), and HotSpot refuses to load
    /// it without this flag — measured on Adoptium 25.0.3.9:
    /// `java.lang.UnsupportedClassVersionError: Preview features are not
    /// enabled for P (class file version 69.65535). Try running with
    /// '--enable-preview'`. CratonVM loaded such a class file unconditionally
    /// until docs/feature-designs/class-resolution-and-caching.md.
    ///
    /// Takes no value, unlike `--enable-native-access` above: HotSpot's flag is
    /// a bare boolean, so it needs no `VALUE_TAKING_OPTS` entry either (see the
    /// comment on that table — boolean flags are copied through verbatim by the
    /// separator inserter).
    #[arg(long = "enable-preview")]
    pub(crate) enable_preview: bool,

    /// AOT compilation mode (-XX:AOTMode=off/training/production).
    #[arg(long = "XX:AOTMode", value_name = "MODE", default_value = "off")]
    pub(crate) aot_mode: String,

    /// AOT cache file path (-XX:AOTCache=path). Used as input in production
    /// mode and as output in training mode (unless AOTCacheOutput is set).
    #[arg(long = "XX:AOTCache", value_name = "PATH")]
    pub(crate) aot_cache: Option<String>,

    /// AOT cache output path (-XX:AOTCacheOutput=path). Overrides AOTCache
    /// for writing in training mode.
    #[arg(long = "XX:AOTCacheOutput", value_name = "PATH")]
    pub(crate) aot_cache_output: Option<String>,

    /// Audit missing native methods: log all ACC_NATIVE methods that were invoked
    /// but had no Rust implementation. Printed on VM shutdown.
    #[arg(long = "XX:AuditMissingNatives")]
    pub(crate) audit_missing_natives: bool,

    /// HotSpot `-XX:±ShowCodeDetailsInExceptionMessages` (JEP 358): route the
    /// non-invoke null-deref opcodes (getfield/putfield/arraylength/array
    /// access/monitor/athrow) through the helpful-NPE message helper. Default
    /// **on**, matching HotSpot (messages verified byte-identical). Tri-state:
    /// absent → use the `VmConfig` default (on); the `-XX:+`/`-XX:-` JDK
    /// spellings are rewritten to `=true`/`=false` (see `rewrite_jvm_args`).
    #[arg(
        long = "XX:ShowCodeDetailsInExceptionMessages",
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    pub(crate) show_code_details_in_exception_messages: Option<bool>,

    /// NEW-10: dump the missing-natives audit log to the given JSON file
    /// on VM shutdown. Implies `--XX:AuditMissingNatives`. The output
    /// schema is `{ "missing_natives": [{class, name, descriptor,
    /// sample_call_site}...] }` with entries sorted so the file is
    /// diff-stable against a committed baseline.
    #[arg(long = "dump-missing-natives", value_name = "FILE")]
    pub(crate) dump_missing_natives: Option<String>,

    /// T2.1.3: dump the missing-natives audit log to the given JSON file,
    /// grouped by JDK module. Implies `--XX:AuditMissingNatives`. Schema:
    /// `{ "version": 1, "modules": { "java.base": [...], "other": [...] } }`.
    /// Entries and module keys are sorted for byte-stable output, so the
    /// file is suitable for committing as a census baseline.
    #[arg(long = "dump-missing-natives-grouped", value_name = "FILE")]
    pub(crate) dump_missing_natives_grouped: Option<String>,

    /// Synthetic-stub census: dump every registered native with its
    /// classification (intrinsic / bridge / synthetic-stub) to the given
    /// JSON file on VM shutdown. Schema (`schema_version` 4):
    /// `{ "mode", "image_adjudication", "counts": {...}, "invocations": {...},
    /// "natives": [{class, name, descriptor, kind, registered_by, overwrote,
    /// invocations, kind_stated, real_declaring_method,
    /// image_declaring_method}...] }`, sorted by
    /// `(class, name, descriptor)` with a stable sort, so duplicate triples
    /// stay in registration order — the overwrite chronology — and the file is
    /// byte-stable across machines. `counts` counts *registrations* (schema 1's
    /// block, unchanged, so the registry census still reads it); `invocations` is
    /// the separate per-kind dispatch total. Use this to verify the default
    /// build is synthetic-stub-free, and (via `invocations`) that no synthetic
    /// stub was dispatched.
    ///
    /// **`invocations` is a LOWER BOUND on calls, not a call count.** It counts
    /// dispatches that resolved the triple by name or id, and misses every
    /// dispatch served from a pre-resolved function pointer — the interpreter's
    /// intrinsic table and the JIT's thin direct-call helpers. MEASURED
    /// 2026-08-17: 100,000 `Math.abs` calls report 1, and the identical run
    /// under `CRATONVM_DISABLE_INTRINSICS=1` reports 100,000; 100,000
    /// `HashMap.get` calls report 1,873 with the JIT on and 100,001 under
    /// `--nojit`. A zero therefore does NOT mean a body is dead. **For a census
    /// whose `invocations` column is exact, run with `--nojit` and
    /// `CRATONVM_DISABLE_INTRINSICS=1`.** `owns_slot` is unaffected and remains
    /// the authoritative answer to "which body would run". Full method,
    /// controls and causal test:
    /// docs/feature-designs/real-jdk-mode.md.
    ///
    /// This flag, like every launcher option, is recognised only BEFORE the
    /// main class; placed after it, it is a program argument. The launcher now
    /// warns when that happens.
    ///
    /// Absolute registration-site paths are redacted, and
    /// `image_declaring_method` — the per-registration adjudication against the
    /// bytes on the class path, which is what tells a real `ACC_NATIVE` bridge
    /// from a registration nobody adjudicated — is `null`, unless
    /// `--explain-real-jdk` is also passed. See
    /// docs/feature-designs/real-jdk-mode.md and
    /// docs/book/src/user-guide/real-jdk-mode.md §9.
    #[arg(long = "dump-native-registry", value_name = "FILE")]
    pub(crate) dump_native_registry: Option<String>,

    /// Phase accounting: write the whole-run wall-clock partition (startup /
    /// class load / compilation / GC / execution / shutdown, plus an explicit
    /// unattributed remainder) to the given JSON file at shutdown, and print
    /// the one-line `[PHASE-ACCOUNTING] …` summary to stderr. Sugar for
    /// `CRATONVM_PHASE_ACCOUNTING=coarse CRATONVM_PHASE_ACCOUNTING_OUT=<FILE>`;
    /// an explicitly-set `CRATONVM_PHASE_ACCOUNTING` (including the
    /// `CRATONVM_DBG=phase-accounting=fine` token spelling) still chooses the
    /// level, so this can be combined with `fine`. Schema and interpretation:
    /// docs/observability/phase-accounting.md.
    ///
    /// The value is consumed by the launcher before the flag snapshot is
    /// latched (see `launcher_phase_report_path`); this field exists so `clap`
    /// accepts the option and `--help` documents it.
    #[arg(long = "dump-phase-report", value_name = "FILE")]
    pub(crate) dump_phase_report: Option<String>,

    /// Enable JDWP debug server on the given port (e.g., 5005).
    /// Equivalent to -agentlib:jdwp=transport=dt_socket,server=y,address=PORT
    #[arg(long = "jdwp-port", value_name = "PORT")]
    pub(crate) jdwp_port: Option<u16>,

    /// Suspend VM at startup waiting for debugger to attach (requires --jdwp-port).
    #[arg(long = "jdwp-suspend")]
    pub(crate) jdwp_suspend: bool,

    // -----------------------------------------------------------------------
    // JPMS module system flags
    // -----------------------------------------------------------------------
    /// Module path: directories and modular JARs to search for modules.
    /// Format: path1;path2 (Windows) or path1:path2 (Unix).
    #[arg(long = "module-path", alias = "p", value_name = "PATH")]
    pub(crate) module_path: Option<String>,

    /// Add a read edge between modules.
    /// Format: `reader_module=target_module[,target_module2,...]`.
    /// Can be specified multiple times.
    #[arg(long = "add-reads", value_name = "MODULE=TARGET")]
    pub(crate) add_reads: Vec<String>,

    /// Export a package from a module to another module (or ALL-UNNAMED).
    /// Format: `module/package=target_module`.
    /// Can be specified multiple times.
    #[arg(long = "add-exports", value_name = "MODULE/PKG=TARGET")]
    pub(crate) add_exports: Vec<String>,

    /// Open a package for deep reflection from a module to another module.
    /// Format: `module/package=target_module`.
    /// Can be specified multiple times.
    #[arg(long = "add-opens", value_name = "MODULE/PKG=TARGET")]
    pub(crate) add_opens: Vec<String>,

    /// Additional root modules to resolve beyond the initial module.
    /// Can be specified multiple times.  `ALL-MODULE-PATH` resolves all
    /// modules found on the module path.
    #[arg(long = "add-modules", value_name = "MODULE")]
    pub(crate) add_modules: Vec<String>,

    /// `--patch-module <module>=<file>(<pathsep><file>)*`: classes and
    /// resources searched before the module's own (interpreter round i4 wave
    /// 87, lane L5). Can be specified multiple times, once per module.
    #[arg(long = "patch-module", value_name = "MODULE=PATH")]
    pub(crate) patch_module: Vec<String>,

    /// `--limit-modules <module>[,<module>...]`: limit the observable modules.
    #[arg(long = "limit-modules", value_name = "MODULES")]
    pub(crate) limit_modules: Vec<String>,

    /// `--upgrade-module-path <path>`: modules that replace upgradeable image
    /// modules.
    #[arg(long = "upgrade-module-path", value_name = "PATH")]
    pub(crate) upgrade_module_path: Option<String>,

    /// Disable container/cgroup support (-XX:-UseContainerSupport).
    /// When set, the JVM ignores cgroup memory/CPU limits.
    #[arg(long = "XX:-UseContainerSupport")]
    pub(crate) disable_container_support: bool,

    /// Garbage-collector selector. Carries the collector name from a HotSpot
    /// `-XX:+Use<name>GC` flag (with the `Use`/`GC` wrapper stripped by
    /// `normalize_java_launcher_argv`), e.g. `G1` for `-XX:+UseG1GC`. CratonVM
    /// honours `G1`, `Z`/`ZGC` and `Generational`; the default (absent this
    /// flag) is `ZGC`, not `Generational` — see `VmConfig::default`. Any other
    /// collector name warns and falls back to `Generational` specifically,
    /// not to the default. Repeated flags follow HotSpot last-wins
    /// (`overrides_with` self → no `ArgumentConflict` on a second occurrence).
    /// See docs/feature-designs/concurrent-gc-maturation.md §3.1.
    #[arg(long = "XX:UseGc", value_name = "NAME", overrides_with = "gc_selector")]
    pub(crate) gc_selector: Option<String>,

    /// `-XX:InitiatingHeapOccupancyPercent=<n>` → G1 IHOP (honoured under G1).
    #[arg(long = "XX:IHOP", value_name = "PCT", overrides_with = "g1_ihop")]
    pub(crate) g1_ihop: Option<String>,

    /// `-XX:MaxDirectMemorySize=<size>` -> direct (off-heap NIO) buffer
    /// accounting cap. Mirrors real JDK: when absent, the cap defaults to
    /// `-Xmx` instead of a fixed value. See
    /// bug-h2-largeblob-direct-memory-oom.md.
    #[arg(
        long = "XX:MaxDirectMemorySize",
        value_name = "SIZE",
        overrides_with = "max_direct_memory"
    )]
    pub(crate) max_direct_memory: Option<String>,

    /// `-XX:MaxMetaspaceSize=<size>` -> class-metadata limit: a define that
    /// would exceed it collects once (class unloading) and then throws
    /// `OutOfMemoryError: Metaspace`, as on HotSpot. Unset = unbounded.
    /// gc-common w6-f; see `ClassManager::metaspace_charged_bytes` for what
    /// is charged.
    #[arg(
        long = "XX:MaxMetaspaceSize",
        value_name = "SIZE",
        overrides_with = "max_metaspace"
    )]
    pub(crate) max_metaspace: Option<String>,
    /// `-XX:MaxJavaStackTraceDepth=<n>` -> the most frames a throwable's stack
    /// trace records (the innermost ones); `0` is unlimited, as on HotSpot.
    /// Default 1024.
    #[arg(
        long = "XX:MaxJavaStackTraceDepth",
        value_name = "N",
        overrides_with = "max_java_stack_trace_depth"
    )]
    pub(crate) max_java_stack_trace_depth: Option<String>,

    /// `-XX:SoftRefLRUPolicyMSPerMB=<n>` -> milliseconds of idleness a
    /// SoftReference is allowed per megabyte of free heap before the LRU policy
    /// may clear it (`VmConfig::soft_ref_lru_policy_ms_per_mb`, every
    /// backend). Absent: 1000, HotSpot's default. gen r5w2/conc6.
    #[arg(
        long = "XX:SoftRefLRUPolicyMSPerMB",
        value_name = "MS",
        overrides_with = "soft_ref_lru_policy_ms_per_mb"
    )]
    pub(crate) soft_ref_lru_policy_ms_per_mb: Option<String>,

    /// gcg g3/x: `-XX:MinHeapSize=<size>` -> HotSpot's minimum heap size
    /// (`VmConfig::min_heap_size`; reported by `[gc,init] Heap Min
    /// Capacity:`). It fell into the silent `-XX:` catch-all until 2026-10-01.
    #[arg(
        long = "XX:MinHeapSize",
        value_name = "SIZE",
        overrides_with = "min_heap_size"
    )]
    pub(crate) min_heap_size: Option<String>,

    /// gcg g3/x: `-XX:InitialHeapSize=<size>` -> the initial heap size, as
    /// `-Xms` but without setting the minimum (HotSpot's distinction). `-Xms`
    /// wins when both are given. Silently dropped until 2026-10-01.
    #[arg(
        long = "XX:InitialHeapSize",
        value_name = "SIZE",
        overrides_with = "initial_heap_size_xx"
    )]
    pub(crate) initial_heap_size_xx: Option<String>,

    /// gcg g3/x: `-XX:MinHeapFreeRatio=<0..100>` (`VmConfig::min_heap_free_ratio`).
    #[arg(
        long = "XX:MinHeapFreeRatio",
        value_name = "PERCENT",
        overrides_with = "min_heap_free_ratio"
    )]
    pub(crate) min_heap_free_ratio: Option<String>,

    /// gcg g3/x: `-XX:MaxHeapFreeRatio=<0..100>` (`VmConfig::max_heap_free_ratio`).
    #[arg(
        long = "XX:MaxHeapFreeRatio",
        value_name = "PERCENT",
        overrides_with = "max_heap_free_ratio"
    )]
    pub(crate) max_heap_free_ratio: Option<String>,

    /// Enable or disable ZGC memory uncommit (per VM).
    #[arg(long = "XX:ZUncommit", overrides_with = "zgc_uncommit")]
    pub(crate) zgc_uncommit: Option<bool>,

    /// Seconds an unused ZGC granule must remain idle before uncommit.
    #[arg(long = "XX:ZUncommitDelay", overrides_with = "zgc_uncommit_delay")]
    pub(crate) zgc_uncommit_delay: Option<String>,

    /// `-XX:SoftMaxHeapSize=<size>` -> ZGC's soft heap maximum (zr z1/tr):
    /// the size its warm-up and allocation-rate rules aim under. Clamped to
    /// `-Xmx`; ignored on the other collectors, as on HotSpot.
    #[arg(
        long = "XX:SoftMaxHeapSize",
        value_name = "SIZE",
        overrides_with = "zgc_soft_max_heap_size"
    )]
    pub(crate) zgc_soft_max_heap_size: Option<String>,

    /// `-XX:+/-ZProactive` -> ZGC's proactive collection rule (zt t6/tc;
    /// HotSpot's default on). Acts under `CRATONVM_ZGC_RATE_TRIGGER=1`.
    #[arg(long = "XX:ZProactive", overrides_with = "zgc_proactive")]
    pub(crate) zgc_proactive: Option<bool>,

    /// `-XX:ZAllocationSpikeTolerance=<f>` -> the allocation-rate rule's
    /// spike tolerance (zt t6/tc; HotSpot's default 2.0). Acts under
    /// `CRATONVM_ZGC_RATE_TRIGGER=1`.
    #[arg(
        long = "XX:ZAllocationSpikeTolerance",
        value_name = "FACTOR",
        overrides_with = "zgc_allocation_spike_tolerance"
    )]
    pub(crate) zgc_allocation_spike_tolerance: Option<String>,

    /// `-XX:ZCollectionInterval=<seconds>` -> ZGC's `Timer` rule (zt t7/tg;
    /// HotSpot's default 0 = off): a collection with cause `Timer` whenever
    /// that long passed since the last one, even on an idle heap.
    #[arg(
        long = "XX:ZCollectionInterval",
        value_name = "SECONDS",
        overrides_with = "zgc_collection_interval"
    )]
    pub(crate) zgc_collection_interval: Option<String>,
    /// `-XX:ZCollectionIntervalMajor=<seconds>` (zt t9/tg2): the flag
    /// `ZCollectionInterval` is HotSpot 25's backwards-compatible alias of;
    /// when given it wins over the alias (`zArguments.cpp` copies the alias
    /// only into a still-default `ZCollectionIntervalMajor`; local 25.0.3
    /// `-XX:+PrintFlagsFinal`).
    #[arg(
        long = "XX:ZCollectionIntervalMajor",
        value_name = "SECONDS",
        overrides_with = "zgc_collection_interval_major"
    )]
    pub(crate) zgc_collection_interval_major: Option<String>,
    /// `-XX:ZCollectionIntervalMinor=<seconds>` (zt t27/gn5, the gn page's
    /// item 14; HotSpot's default -1 = off): ZGC's minor `Timer` rule, a young
    /// collection with cause `Timer` whenever that long passed since the last
    /// collection. Acts only in ZGC's generational mode
    /// (`CRATONVM_ZGC_GENERATIONAL=1`); dropped silently before.
    #[arg(
        long = "XX:ZCollectionIntervalMinor",
        value_name = "SECONDS",
        overrides_with = "zgc_collection_interval_minor"
    )]
    pub(crate) zgc_collection_interval_minor: Option<String>,
    /// `-XX:+/-ZCollectionIntervalOnly` (zt t10/tg3; HotSpot's default
    /// off): only the `Timer` rule starts a ZGC collection. Honoured unless
    /// `CRATONVM_ZGC_COLLECTION_INTERVAL_ONLY=0`.
    #[arg(long = "XX:ZCollectionIntervalOnly", overrides_with = "zgc_collection_interval_only")]
    pub(crate) zgc_collection_interval_only: Option<bool>,
    /// zt t7/x4b: the launcher's own spelling of a ZGC boolean `-XX:` flag
    /// given without its `+`/`-` (`-XX:ZProactive=false`, `-XX:ZUncommit`),
    /// in command-line order ([`zgc_boolean_without_sign`]). Not a user flag.
    #[arg(long = "XX:ZBooleanWithoutSign", value_name = "OPTION", hide = true)]
    pub(crate) zgc_boolean_without_sign: Vec<String>,

    /// `-XX:G1HeapRegionSize=<bytes>` → G1 region size (honoured under G1).
    #[arg(
        long = "XX:G1RegionSize",
        value_name = "SIZE",
        overrides_with = "g1_region_size"
    )]
    pub(crate) g1_region_size: Option<String>,

    /// `-XX:MaxGCPauseMillis=<n>` → pause target. G1 sizes its mixed
    /// collection set from it; ZGC (the default collector) sizes its
    /// allocation budget from it, since 2026-09-03. The generational
    /// backend ignores it.
    #[arg(
        long = "XX:MaxGCPause",
        value_name = "MS",
        overrides_with = "g1_max_pause"
    )]
    pub(crate) g1_max_pause: Option<String>,

    /// `-XX:G1MixedGCLiveThresholdPercent=<n>` → an Old region at or above
    /// this percent live is never a mixed-collection candidate (honoured
    /// under G1).
    #[arg(
        long = "XX:G1MixedLiveThreshold",
        value_name = "PCT",
        overrides_with = "g1_mixed_live_threshold"
    )]
    pub(crate) g1_mixed_live_threshold: Option<String>,

    /// `-XX:G1HeapWastePercent=<n>` → the mixed phase ends once its candidates
    /// hold less garbage than this percent of the heap (honoured under G1).
    #[arg(
        long = "XX:G1HeapWaste",
        value_name = "PCT",
        overrides_with = "g1_heap_waste"
    )]
    pub(crate) g1_heap_waste: Option<String>,

    /// `-XX:ParallelGCThreads=<n>` → GC evacuation worker count (honoured
    /// under G1). Absent means the count is derived from the machine.
    #[arg(
        long = "XX:ParallelGCThreads",
        value_name = "N",
        overrides_with_all = ["g1_parallel_gc_threads", "zgc_parallel_gc_threads"]
    )]
    pub(crate) g1_parallel_gc_threads: Option<String>,

    /// Historical normalized spelling; both worker-count options now apply
    /// to G1 and ZGC. Keep it as a hidden alias for existing invocations.
    #[arg(
        long = "XX:ZgcParallelGCThreads",
        value_name = "N",
        hide = true,
        overrides_with_all = ["zgc_parallel_gc_threads", "g1_parallel_gc_threads"]
    )]
    pub(crate) zgc_parallel_gc_threads: Option<String>,

    /// zr z3/mt: `-XX:ConcGCThreads=<n>` -> ZGC's concurrent mark workers.
    #[arg(
        long = "XX:ConcGCThreads",
        value_name = "N",
        overrides_with = "conc_gc_threads"
    )]
    pub(crate) conc_gc_threads: Option<String>,

    /// zr z3/mt: `-XX:+/-UseDynamicNumberOfGCThreads` (`true` / `false`).
    #[arg(
        long = "XX:UseDynamicNumberOfGCThreads",
        value_name = "BOOL",
        overrides_with = "use_dynamic_gc_threads"
    )]
    pub(crate) use_dynamic_gc_threads: Option<bool>,

    /// `-XX:±UseStringDeduplication` → G1 String dedup (honoured under G1).
    #[arg(
        long = "XX:StringDedup",
        value_name = "BOOL",
        overrides_with = "g1_string_dedup"
    )]
    pub(crate) g1_string_dedup: Option<String>,

    /// Unified logging spec (-Xlog:tag[+tag]*[=level][:output[:decorators]]).
    /// Example: --Xlog gc*=info:stdout:time,level,tags
    ///
    /// Repeatable, as HotSpot's `-Xlog` is (`-Xlog:gc -Xlog:safepoint`): a
    /// second `-Xlog` made clap refuse the command line ("cannot be used
    /// multiple times"). The options are joined with `;`, the option
    /// separator `unified_logging::UnifiedLogger::parse` splits on, in
    /// command-line order (a later option overrides an earlier one, as on
    /// HotSpot). gen r5w3/obs7.
    #[arg(long = "Xlog", value_name = "SPEC")]
    pub(crate) xlog: Vec<String>,

    /// T19.H1 — if set, spawn a watchdog thread that, after `SECONDS`,
    /// signals every interpreter thread to dump its frame chain to
    /// stderr and then calls `std::process::abort()`. Used to
    /// diagnose silent-hang bootstraps (Keycloak, WildFly, Quarkus).
    /// The flag is honoured on a best-effort basis — threads stuck
    /// inside Rust native code will not dump (the watchdog still
    /// aborts with a reduced-information banner in that case).
    #[arg(long = "stack-dump-on-timeout", value_name = "SECONDS")]
    pub(crate) stack_dump_on_timeout: Option<u64>,

    /// READ THIS BEFORE AGGREGATING BY METHOD NAME: a leaf frame at
    /// `pc=0 last_pc=0` has executed NOTHING, and the time it represents
    /// belongs to the **invoke that pushed it**, not to its body. The hook
    /// that emits a sample sits at the top of the dispatch loop, and an
    /// invoke pushes the callee frame and `continue`s — so the first
    /// iteration able to observe a re-armed request after an expensive
    /// invoke reports the callee at its entry. Bucket those separately or an
    /// invoke-dense workload reads as "the callee body is slow". Calibrated
    /// by `probes/InvokeAttributionProbe.java`, where a three-bytecode callee
    /// takes 54% of samples at `pc=0` and one sample anywhere else; three
    /// profiles on the Tomcat annotation-scan page were misread this way.
    ///
    /// If set, sample every interpreter thread's Java frame chain to stderr
    /// every `MILLIS` and keep running (no abort). Unlike
    /// `--stack-dump-on-timeout`, which emits one dump per nested interpreter
    /// entry and therefore ranks methods by CALL COUNT, this is a
    /// time-weighted profile: aggregate the leaf frame of each emitted
    /// `T19.H1 stack dump` record to see where wall-clock actually goes.
    /// Diagnostic-only. JIT-compiled frames never reach the dispatch loop and
    /// so are not sampled — pair it with `--nojit`, or read the result as
    /// "of the interpreted time, ...".
    #[arg(long = "stack-sample-ms", value_name = "MILLIS")]
    pub(crate) stack_sample_ms: Option<u64>,

    // -----------------------------------------------------------------------
    // GPU offload (see docs/gpu/cuda-oxide-evaluation.md)
    //
    // Every field below is gated behind the `gpu` Cargo feature. Without
    // the feature the flags are not parsed, not documented in --help,
    // and the CPU execution path is byte-identical to before the GPU
    // work landed.
    // -----------------------------------------------------------------------
    /// Enable GPU offload of eligible static methods. Requires the
    /// CLI to be built with `--features gpu` and a CUDA driver. With
    /// no driver, the flag is honoured but no methods are offloaded.
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu")]
    pub(crate) gpu: bool,

    /// Select CUDA device ordinal when --gpu is on. Defaults to 0.
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-device", value_name = "N", default_value_t = 0)]
    pub(crate) gpu_device: u32,

    /// Minimum estimated work (array length / loop trip count) before
    /// a method is offloaded. Smaller inputs run on the CPU because
    /// the host↔device round-trip dominates.
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-min-work", value_name = "N", default_value_t = 4096)]
    pub(crate) gpu_min_work: u32,

    /// Print one line per analyzer verdict (Eligible / Rejected) at
    /// INFO level. Useful for understanding why a method did or did
    /// not offload.
    #[cfg(feature = "gpu")]
    #[arg(long = "print-gpu-decisions")]
    pub(crate) print_gpu_decisions: bool,

    /// Probe the GPU, print device name + compute capability + memory,
    /// then exit. Useful for sanity-checking before a real run.
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-info")]
    pub(crate) gpu_info: bool,

    /// Per-VM soft VRAM quota (e.g. `256m`, `2g`; `0` = inherit default).
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-vram-soft-quota", value_name = "SIZE")]
    pub(crate) gpu_vram_soft_quota: Option<String>,

    /// Per-VM hard VRAM quota (e.g. `512m`, `4g`; `0` = unlimited).
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-vram-hard-quota", value_name = "SIZE")]
    pub(crate) gpu_vram_hard_quota: Option<String>,

    /// Per-VM soft pinned host memory quota (e.g. `128m`; `0` = unlimited).
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-pin-soft-quota", value_name = "SIZE")]
    pub(crate) gpu_pin_soft_quota: Option<String>,

    /// Per-VM hard pinned host memory quota (e.g. `256m`; `0` = unlimited).
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-pin-hard-quota", value_name = "SIZE")]
    pub(crate) gpu_pin_hard_quota: Option<String>,

    /// Per-VM maximum cached spans in HostRegionPinCache (`0` = unlimited).
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-pin-max-spans", value_name = "N")]
    pub(crate) gpu_pin_max_spans: Option<usize>,

    /// Admit approximate `Math.exp(double)` → `ex2.approx.f32` in GPU offload
    /// kernels (opt-in; off by default).
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-approx-math")]
    pub(crate) gpu_approx_math: bool,

    /// Arguments passed to the Java program's main method.
    #[arg(trailing_var_arg = true)]
    pub(crate) args: Vec<String>,
}

/// Validate that `name` is a valid Java class name in internal (slash) notation.
///
/// Each segment (split by `/`) must:
/// - Not be empty
/// - Start with a letter, underscore, or dollar sign
/// - Contain only alphanumeric characters, underscores, or dollar signs
///
/// "Letter" and "alphanumeric" are Unicode's, as a Java identifier's are
/// (`Character.isJavaIdentifierStart` / `Part`): `Café` is a class HotSpot
/// launches (interpreter round i2 wave 59, lane L5; probe
/// `L5W59LauncherClassNames`). A name this rejects is no class the loader
/// could answer; the launcher reports it as HotSpot does, a
/// `ClassNotFoundException` in `java.launcher.cls.error1`.
pub(crate) fn validate_class_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Class name must not be empty");
    }
    for (i, segment) in name.split('/').enumerate() {
        if segment.is_empty() {
            bail!(
                "Invalid class name '{}': segment {} is empty (double slash or leading/trailing slash)",
                name,
                i + 1
            );
        }
        let first = match segment.chars().next() {
            Some(ch) => ch,
            None => continue, // skip empty segments (already caught above, but be safe)
        };
        if !first.is_alphabetic() && first != '_' && first != '$' {
            bail!(
                "Invalid class name '{}': segment '{}' must start with a letter, underscore, or dollar sign",
                name,
                segment
            );
        }
        for ch in segment.chars() {
            if !ch.is_alphanumeric() && ch != '_' && ch != '$' {
                bail!(
                    "Invalid class name '{}': segment '{}' contains invalid character '{}'",
                    name,
                    segment,
                    ch
                );
            }
        }
    }
    Ok(())
}

/// Expand missing aggregate JAR references into sibling split JARs.
///
/// Some libraries ship as a single "all-in-one" fat JAR (e.g.
/// `netty-all.jar`, `groovy-all.jar`) or, alternatively, as a set of split
/// modules that live next to it (e.g. `netty-common.jar`, `netty-buffer.jar`,
/// …).  When a user passes a classpath entry that points at the aggregate JAR
/// but the repository only contains the split distribution, the classloader
/// silently drops the entry and the program fails with a puzzling
/// `NoClassDefFoundError`.
///
/// This helper detects that situation for a fixed set of known prefixes
/// (currently `netty`) and substitutes the aggregate name with every sibling
/// split JAR that shares the prefix.  The substitution only happens when the
/// aggregate file does *not* exist on disk; if it exists the entry is left
/// untouched.
///
/// Returns a new classpath list with the substitutions applied.  A warning is
/// emitted to stderr when a substitution occurs so the user knows what
/// happened.
pub(crate) fn expand_aggregate_jars(entries: Vec<String>) -> Vec<String> {
    // (aggregate_file_name, split_prefix) pairs.  The split prefix is
    // matched case-insensitively against sibling file names.
    const AGGREGATES: &[(&str, &str)] = &[("netty-all.jar", "netty-")];

    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        let path = std::path::Path::new(&entry);
        // PERF: the aggregate-jar case is rare, so do a cheap filename match
        // before touching the filesystem. Entries whose file name is not a
        // known aggregate name can never be substituted, so they skip the
        // `exists()` stat syscall (and the `read_dir` below) entirely.
        let file_name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_ascii_lowercase(),
            None => {
                out.push(entry);
                continue;
            }
        };
        let matched = AGGREGATES.iter().find(|(name, _)| file_name == *name);
        let Some((_, split_prefix)) = matched else {
            out.push(entry);
            continue;
        };
        // Only now (for a candidate aggregate name) pay for the stat syscall.
        // If the entry already exists on disk (or is a directory), keep it.
        if path.exists() {
            out.push(entry);
            continue;
        }
        // Look for split jars next to where the aggregate was expected to be.
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => std::path::PathBuf::from("."),
        };
        let Ok(reader) = std::fs::read_dir(&parent) else {
            out.push(entry);
            continue;
        };
        let mut substitutes: Vec<String> = Vec::new();
        for dirent in reader.flatten() {
            let p = dirent.path();
            if p.extension().map(|e| e == "jar").unwrap_or(false) {
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    if name.to_ascii_lowercase().starts_with(split_prefix)
                        && name.to_ascii_lowercase() != file_name
                    {
                        substitutes.push(p.to_string_lossy().into_owned());
                    }
                }
            }
        }
        if substitutes.is_empty() {
            // Nothing to substitute — preserve original entry so downstream
            // logging surfaces the missing file.
            out.push(entry);
            continue;
        }
        // Deterministic order helps reproducibility.
        substitutes.sort();
        eprintln!(
            "Note: classpath entry {entry} not found; substituting {n} sibling '{split_prefix}*.jar' files from {parent}",
            n = substitutes.len(),
            parent = parent.display()
        );
        out.extend(substitutes);
    }
    out
}

pub(crate) struct StagedArchiveCleanup {
    pub(crate) path: std::path::PathBuf,
}

pub(crate) static STAGED_ARCHIVE_COPIES: std::sync::OnceLock<std::sync::Mutex<Vec<std::path::PathBuf>>> =
    std::sync::OnceLock::new();

pub(crate) fn staged_archive_copies() -> &'static std::sync::Mutex<Vec<std::path::PathBuf>> {
    STAGED_ARCHIVE_COPIES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

pub(crate) fn lock_staged_archive_copies(
    registry: &'static std::sync::Mutex<Vec<std::path::PathBuf>>,
) -> std::sync::MutexGuard<'static, Vec<std::path::PathBuf>> {
    match registry.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub(crate) fn remove_staged_archive_copy(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                "failed to remove staged archive copy {}: {e}",
                path.display()
            );
        }
    }
}

pub(crate) fn register_staged_archive_copy(path: std::path::PathBuf) {
    lock_staged_archive_copies(staged_archive_copies()).push(path);
}

pub(crate) fn unregister_staged_archive_copy(path: &std::path::Path) {
    if let Some(registry) = STAGED_ARCHIVE_COPIES.get() {
        let mut guard = lock_staged_archive_copies(registry);
        guard.retain(|registered| registered.as_path() != path);
    }
}

pub(crate) fn cleanup_staged_archive_copies() {
    if let Some(registry) = STAGED_ARCHIVE_COPIES.get() {
        let paths: Vec<_> = {
            let mut guard = lock_staged_archive_copies(registry);
            guard.drain(..).collect()
        };
        for path in paths {
            remove_staged_archive_copy(&path);
        }
    }
}

impl Drop for StagedArchiveCleanup {
    fn drop(&mut self) {
        unregister_staged_archive_copy(&self.path);
        remove_staged_archive_copy(&self.path);
    }
}

pub(crate) fn classpath_entry_for_archive(
    archive_path: &std::path::Path,
) -> Result<(std::path::PathBuf, Option<StagedArchiveCleanup>)> {
    if archive_path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
    {
        return Ok((archive_path.to_path_buf(), None));
    }

    // JN3: ClassPath::new only accepts entries whose extension is `.jar`
    // (or `.jmod`/`modules`). WARs, EARs, and other Java archive types are
    // silently dropped, so stage a temporary `.jar` copy that remains alive
    // until the launcher returns.
    let stem = archive_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("app");
    let pid = std::process::id();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = std::env::temp_dir();
    let mut tmp = dir.join(format!("cratonvm-{pid}-{now_ms}-{stem}.jar"));
    let mut dst_file = None;
    for attempt in 0..16 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => {
                dst_file = Some(f);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                tmp = dir.join(format!("cratonvm-{pid}-{now_ms}-{attempt}-{stem}.jar"));
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "failed to stage {} as {} for classpath registration",
                    archive_path.display(),
                    tmp.display()
                )));
            }
        }
    }
    let mut dst_file = dst_file.ok_or_else(|| {
        anyhow::anyhow!(
            "failed to stage {} for classpath registration: \
             could not create a unique temp file in {}",
            archive_path.display(),
            dir.display()
        )
    })?;
    let copy_result = (|| -> Result<()> {
        let mut src_file = std::fs::File::open(archive_path)
            .with_context(|| format!("failed to open {} for staging", archive_path.display()))?;
        std::io::copy(&mut src_file, &mut dst_file).with_context(|| {
            format!(
                "failed to stage {} as {} for classpath registration",
                archive_path.display(),
                tmp.display()
            )
        })?;
        Ok(())
    })();
    drop(dst_file);
    if let Err(e) = copy_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    tracing::debug!(
        "JN3: staged non-.jar archive {} в†’ {} so ClassPath accepts it",
        archive_path.display(),
        tmp.display()
    );
    register_staged_archive_copy(tmp.clone());
    Ok((tmp.clone(), Some(StagedArchiveCleanup { path: tmp })))
}

/// Launcher options that consume the *following* argv token as their value
/// (the `--opt value` form). Needed by [`insert_program_args_separator`] so a
/// value token (e.g. the classpath string after `-cp`) is not mistaken for the
/// bare main-class name that selects the program. Both the HotSpot spellings
/// (`-jar`, `-classpath`, `-cp`, `-p`, `-mp`) and the clap long spellings
/// (`--jar`, `--classpath`, ...) are listed because this runs before
/// `normalize_java_launcher_argv` rewrites them.
///
/// Options using the inline `--opt=value` form need no entry here -- the value
/// travels in the same token. Boolean flags also need no entry.
pub(crate) const VALUE_TAKING_OPTS: &[&str] = &[
    "--XX:ParallelGCThreads",
    "--XX:ZgcParallelGCThreads",
    "-jar",
    "--jar",
    "-classpath",
    "-cp",
    "--classpath",
    // zt t12/tg5: the JDK 9+ long spelling (normalised to `--classpath`).
    "--class-path",
    "-c",
    "-p",
    "--module-path",
    "-mp",
    // HotSpot single-dash forms used as separate tokens (`-Xmx 256m`,
    // `-Xshare on`, etc.). Most users write them inline (`-Xmx256m`),
    // but Maven Surefire and some test harnesses split them. Listing
    // them here keeps the separator-inserter from mistaking the value
    // token for a bare main-class name.
    "-Xmx",
    "-Xms",
    // gen r4w2/alloc2: `-Xmn 256m` (separate token) must not leave `256m`
    // behind as the main-class candidate.
    "-Xmn",
    "-Xshare",
    "-Xverify",
    "-Xbootclasspath",
    // NOT `-Xlog`: HotSpot's `-Xlog` takes its spec only after a colon, and a
    // bare `-Xlog` is `-Xlog:all` (gen r5w4/obs8,
    // `gengc-r5w3-obs7-xlog-bare-option-eats-the-next-argument`). Listing it
    // here made `java -Xlog Main` take `Main` as the log spec.
    "--Xmx",
    "--Xms",
    "--Xmn",
    "--Xbootclasspath",
    "--Xbootclasspath-append",
    "--Xbootclasspath-prepend",
    "--java-home",
    "--Xverify",
    "--XX:SharedArchiveFile",
    "--XX:UseGc",
    "--Xshare",
    "--XX:AOTMode",
    "--XX:AOTCache",
    "--XX:AOTCacheOutput",
    "--dump-missing-natives",
    "--dump-missing-natives-grouped",
    "--dump-native-registry",
    // Real-JDK mode (docs/book/src/user-guide/real-jdk-mode.md §9). Both take a
    // path; `--real-jdk`, `--trace-real-jdk` and `--explain-real-jdk` are
    // booleans and need no entry.
    "--real-jdk-report",
    "--dump-class-origins",
    "--jdwp-port",
    "--add-reads",
    "--add-exports",
    "--add-opens",
    "--add-modules",
    // Interpreter round i4 wave 87, lane L5.
    "--patch-module",
    "--limit-modules",
    "--upgrade-module-path",
    "--Xlog",
    "--stack-dump-on-timeout",
    "--stack-sample-ms",
    "--gpu-device",
    "--gpu-min-work",
    "--gpu-vram-soft-quota",
    "--gpu-vram-hard-quota",
    "--gpu-pin-soft-quota",
    "--gpu-pin-hard-quota",
    "--gpu-pin-max-spans",
    "--source",
];

/// Expand Java argument files (`@<path>`).
///
/// When the JVM launcher sees an argument starting with `@` (not `@@`),
/// it reads the file at that path and expands its contents as additional
/// command-line arguments. This is Java's argument-file feature (JEP 293,
/// available since Java 9). Gradle uses it to pass large classpaths via
/// `@classpath-file.txt` to avoid command-line length limits.
///
/// The file format:
/// - Arguments separated by whitespace (spaces, tabs, newlines)
/// - `"..."` or `'...'` quoted strings (quotes stripped, content preserved)
/// - `#` starts a comment to end of line
/// - Backslash escapes the next character inside quotes
/// - `@@path` is a literal `@path` (single-expansion escape)
///
/// Expansion is NOT recursive (nested `@file` references inside the
/// expanded content are left as-is) to avoid runaway expansion.
/// The `args[0]` element (program name) is never expanded.
pub(crate) fn expand_argfiles(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return args;
    }
    let mut out = vec![args[0].clone()]; // preserve argv[0]
    for arg in &args[1..] {
        if let Some(path_str) = arg.strip_prefix('@') {
            if path_str.starts_with('@') {
                // `@@path` -> literal `@path`
                out.push(path_str.to_string());
                continue;
            }
            match std::fs::read_to_string(path_str) {
                Ok(content) => {
                    // Tokenize the file contents
                    out.extend(tokenize_argfile(&content));
                    if None::<std::ffi::OsString>.is_some() {
                        eprintln!("[cratonvm] @-expanded {path_str}: {} tokens", out.len());                    }                }
                Err(e) => {
                    // If the file can't be read, leave the @arg as-is so
                    // downstream stages can produce a clear error message.
                    if None::<std::ffi::OsString>.is_some() {
                        eprintln!("[cratonvm] @-file read error {path_str}: {e}");                    }
                    out.push(arg.clone());                }            }        } else {
            out.push(arg.clone());        }    }
    out
}

/// Tokenize the content of a Java argument file.
///
/// Splits on unquoted whitespace; `"..."` and `'...'` preserve whitespace
/// and strip the outer quotes; `#` starts a comment to end of line;
/// backslash inside quotes escapes the following character.
pub(crate) fn tokenize_argfile(content: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '#' => {
                // Comment: skip to end of line
                if current.is_empty() {
                    // Flush any pending token
                } else {
                    tokens.push(std::mem::take(&mut current));
                }
                for ch2 in chars.by_ref() {
                    if ch2 == '\n' {
                        break;
                    }
                }
            }
            '"' | '\'' => {
                // Quoted string: collect until matching quote
                let quote = ch;
                loop {
                    match chars.next() {
                        None => break,
                        Some('\\') if quote == '"' => {
                            // Backslash escape inside double-quotes
                            if let Some(escaped) = chars.next() {
                                current.push(escaped);
                            }
                        }
                        Some(c) if c == quote => break,
                        Some(c) => current.push(c),
                    }
                }
            }
            ' ' | '\t' | '\r' | '\n' => {
                // Whitespace: flush token
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// The environment variable the JDK 9+ `java` launcher prepends to its
/// command line (JEP 293; libjli `JLI_AddArgsFromEnvVar`).
pub(crate) const JDK_JAVA_OPTIONS: &str = "JDK_JAVA_OPTIONS";

/// zt t14/x11 (`zgc-zt13x10-review-residuals-CLOSED-20261019.md` item 16,
/// `CRATONVM_LAUNCHER_JDK_JAVA_OPTIONS`, default ON, `=0` kill; every
/// collector, both modes): is `JDK_JAVA_OPTIONS` read? The launcher needs
/// the answer while it builds the argv the flag snapshot's own overrides
/// come from (`main`'s early pass: `--nojit`, `-ea`), before that snapshot
/// exists, so the switch is read from the process environment directly with
/// the snapshot's off-word rule (`0`, `false`, `off`, `no`).
pub(crate) fn jdk_java_options_enabled() -> bool {
    None::<std::ffi::OsString>
        .is_none_or(|v| !cratonvm_types::flags::value_is_off(&v.to_string_lossy()))
}

/// The launch argv: `args[0]`, then the tokens of `JDK_JAVA_OPTIONS`
/// ([`jdk_java_options_tokens`]), then the command line's own arguments,
/// `@argfiles` expanded ([`expand_argfiles`]). With the variable unset or
/// the switch off: [`expand_argfiles`] alone, as before. Every launcher stage
/// reads this argv, so the variable's options meet the same parse as the
/// command line's (the missing-value scan, the module launch, the separator,
/// `hotspot_jvm_args` for `getInputArguments`), as libjli's merged list does.
///
/// Exits the process with HotSpot's status 1 on the refusals
/// [`jdk_java_options_tokens`] makes. The `NOTE:` line is printed once per
/// process (the launcher builds this argv twice: `main`'s early pass, then
/// `run`).
pub(crate) fn launch_argv(args: Vec<String>) -> Vec<String> {
    let Some(value) = std::env::var_os(JDK_JAVA_OPTIONS) else {
        return expand_argfiles(args);
    };
    if args.is_empty() || !jdk_java_options_enabled() {
        return expand_argfiles(args);
    }
    let value = value.to_string_lossy().into_owned();
    static NOTE: std::sync::Once = std::sync::Once::new();
    NOTE.call_once(|| eprintln!("NOTE: Picked up {JDK_JAVA_OPTIONS}: {value}"));
    let env_tokens = match jdk_java_options_tokens(&value, |path| std::fs::read_to_string(path).ok())
    {
        Ok(tokens) => tokens,
        Err(refusal) => {
            eprintln!("{refusal}");
            let _ = std::io::Write::flush(&mut std::io::stderr());
            std::process::exit(1);
        }
    };
    let mut expanded = expand_argfiles(args).into_iter();
    let mut out: Vec<String> = expanded.next().into_iter().collect();
    out.extend(env_tokens);
    out.extend(expanded);
    out
}

/// zt t14/x11: the tokens of a `JDK_JAVA_OPTIONS` value, or the refusal
/// HotSpot's launcher prints for it (exit status 1), per libjli `args.c`
/// (`expand` with its environment-variable arm):
///
/// - tokens are split at white space; a `"` or `'` starts a quoted run
///   inside a token, which ends at the same quote and keeps its white space
///   (no escapes, no comments: the `@argfile` rules do not apply to the
///   variable itself). An unclosed quote: `Error: unmatched quote in
///   environment variable JDK_JAVA_OPTIONS`;
/// - an `@argfile` token is expanded as on the command line
///   ([`tokenize_argfile`]); `@@x` is the literal `@x`; an argfile that
///   cannot be read ends the launch with libjli's `Error: could not open
///   `<file>'` (zt t15/x12; [`expand_argfiles`] still keeps one on the
///   command line);
/// - a terminal option (`-jar`, `-m`, `--module`, `--module=..`, `--dry-run`,
///   the help, `-X` and version options): `Error: Option <opt> is not allowed
///   in environment variable JDK_JAVA_OPTIONS`, or `Error: Option <opt> in
///   @<file> is not allowed in environment variable JDK_JAVA_OPTIONS` when
///   it came from an argfile;
/// - a main class (a token that is not an option and not the value of the
///   white-space option before it, `checkArg`): `Error: Cannot specify main
///   class in environment variable JDK_JAVA_OPTIONS`.
///
/// `read` reads an argfile (`None`: unreadable). Pure but for `read`.
pub(crate) fn jdk_java_options_tokens(
    value: &str,
    read: impl Fn(&str) -> Option<String>,
) -> Result<Vec<String>, String> {
    let var = JDK_JAVA_OPTIONS;
    let mut out: Vec<String> = Vec::new();
    // libjli `checkArg`'s `expectingNoDashArg`: the previous token was an
    // option that takes the next one as its value.
    let mut expecting_value = false;
    for token in split_jdk_java_options(value)
        .ok_or_else(|| format!("Error: unmatched quote in environment variable {var}"))?
    {
        let (args, from_file): (Vec<String>, Option<&str>) = match token.strip_prefix('@') {
            Some(rest) if rest.starts_with('@') => (vec![rest.to_string()], None),
            // libjli `JLI_PreprocessArg`: `@` by itself is an argument.
            Some("") => (vec![token.clone()], None),
            Some(path) => match read(path) {
                Some(content) => (tokenize_argfile(&content), Some(token.as_str())),
                // zt t15/x12 (review of t14/x11): unreadable, libjli's
                // `expandArgFile` ends the launch (`CFG_ERROR6`, exit 1); it
                // was kept as its `@` token and later read as a main class.
                None => return Err(format!("Error: could not open `{path}'")),
            },
            None => (vec![token.clone()], None),
        };
        // libjli checks every element of the token for a terminal option
        // first, and for a main class only after the whole token.
        if let Some(arg) = args.iter().find(|a| jdk_java_options_terminal(a.as_str())) {
            return Err(match from_file {
                Some(file) => format!(
                    "Error: Option {arg} in {file} is not allowed in environment variable {var}"
                ),
                None => format!("Error: Option {arg} is not allowed in environment variable {var}"),
            });
        }
        let mut main_class = false;
        for arg in &args {
            if arg.starts_with('-') {
                expecting_value = jdk_java_options_takes_value(arg);
            } else if expecting_value {
                expecting_value = false;
            } else {
                main_class = true;
            }
        }
        if main_class {
            return Err(format!(
                "Error: Cannot specify main class in environment variable {var}"
            ));
        }
        out.extend(args);
    }
    Ok(out)
}

/// zt t14/x11: libjli `expand`'s environment-variable split. `None`: an
/// unclosed quote. White space is C's `isspace`.
pub(crate) fn split_jdk_java_options(value: &str) -> Option<Vec<String>> {
    let space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{0b}' | '\u{0c}');
    let mut tokens = Vec::new();
    let mut chars = value.chars().peekable();
    loop {
        while chars.peek().is_some_and(|&c| space(c)) {
            chars.next();
        }
        if chars.peek().is_none() {
            return Some(tokens);
        }
        let mut token = String::new();
        while let Some(&c) = chars.peek() {
            if space(c) {
                break;
            }
            chars.next();
            if c == '"' || c == '\'' {
                loop {
                    match chars.next() {
                        None => return None,
                        Some(q) if q == c => break,
                        Some(other) => token.push(other),
                    }
                }
            } else {
                token.push(c);
            }
        }
        tokens.push(token);
    }
}

/// zt t14/x11: libjli `isTerminalOpt`, the options `JDK_JAVA_OPTIONS` may
/// not carry.
pub(crate) fn jdk_java_options_terminal(arg: &str) -> bool {
    matches!(
        arg,
        "-jar"
            | "-m"
            | "--module"
            | "--dry-run"
            | "-h"
            | "-?"
            | "-help"
            | "--help"
            | "-X"
            | "--help-extra"
            | "-version"
            | "--version"
            | "-fullversion"
            | "--full-version"
    ) || arg.starts_with("--module=")
}

/// zt t14/x11: libjli `IsWhiteSpaceOption` (a module option, a class-path
/// option, `--describe-module` / `-d`, `--source`): the next token is this
/// option's value, not a main class.
pub(crate) fn jdk_java_options_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "--module-path"
            | "-p"
            | "--upgrade-module-path"
            | "--add-modules"
            | "--enable-native-access"
            | "--limit-modules"
            | "--add-exports"
            | "--add-opens"
            | "--add-reads"
            | "--patch-module"
            | "-classpath"
            | "-cp"
            | "--class-path"
            | "--module"
            | "-m"
            | "--describe-module"
            | "-d"
            | "--source"
    )
}

/// The JVM options HotSpot would record in `Arguments::jvm_args()` for this
/// command line (interpreter round i4 wave 84, lane L1;
/// `VmConfig::jvm_args`, the perf-data file's `java.rt.vmArgs`, `jps -v`).
/// `argv` is the launcher's argv after [`insert_program_args_separator`]:
/// only the leading option section (up to `--`) is read. The JDK's `java`
/// launcher hands `JNI_CreateJavaVM` every VM option in order, rewrites a
/// separate-value module option (`--add-opens X`, `-p X`) to `--opt=X`
/// (`AddLongFormOption`) and the class path to `-Djava.class.path`, and
/// `parse_each_vm_init_arg` records all of them but `-Djava.class.path`,
/// `-Dsun.java.command` and `-Dsun.java.launcher*`. Launcher-only options
/// (`-cp`, `-jar`, `-version`, `-XshowSettings`, this launcher's own `--`
/// options) and the program selector are not VM options.
pub(crate) fn hotspot_jvm_args(argv: &[String]) -> Vec<String> {
    const MODULE_OPTIONS: [&str; 12] = [
        "--module-path",
        "--upgrade-module-path",
        "--add-modules",
        "--limit-modules",
        "--add-exports",
        "--add-opens",
        "--add-reads",
        "--patch-module",
        "--enable-native-access",
        "--illegal-native-access",
        "--sun-misc-unsafe-memory-access",
        "--enable-final-field-mutation",
    ];
    const SINGLE_DASH: [&str; 10] = [
        "-ea",
        "-da",
        "-esa",
        "-dsa",
        "-enableassertions",
        "-disableassertions",
        "-enablesystemassertions",
        "-disablesystemassertions",
        "-verbose",
        "--enable-preview",
    ];
    let mut out = Vec::new();
    let mut i = 1usize;
    while i < argv.len() {
        let a = argv[i].as_str();
        if a == "--" {
            break;
        }
        if matches!(a, "-cp" | "-classpath" | "--class-path" | "-jar") {
            i += 2;
            continue;
        }
        let module_option = if a == "-p" {
            Some("--module-path")
        } else {
            MODULE_OPTIONS.iter().copied().find(|o| *o == a)
        };
        if let Some(name) = module_option {
            if let Some(value) = argv.get(i + 1) {
                out.push(format!("{name}={value}"));
            }
            i += 2;
            continue;
        }
        let vm_option = if let Some(property) = a.strip_prefix("-D") {
            !(property.starts_with("java.class.path=")
                || property.starts_with("sun.java.command=")
                || property.starts_with("sun.java.launcher"))
        } else if a.starts_with("-X") {
            !a.starts_with("-XshowSettings") && a != "-X"
        } else {
            a.starts_with("-agentlib:")
                || a.starts_with("-agentpath:")
                || a.starts_with("-javaagent:")
                // zt t18/hunt2: a VM option HotSpot records like any other.
                || a.starts_with("--finalization=")
                || MODULE_OPTIONS
                    .iter()
                    .any(|o| a.strip_prefix(o).is_some_and(|rest| rest.starts_with('=')))
                || SINGLE_DASH
                    .iter()
                    .any(|o| a == *o || a.strip_prefix(o).is_some_and(|rest| rest.starts_with(':')))
        };
        if vm_option {
            out.push(a.to_string());
        }
        i += 1;
    }
    out
}

/// Enforce `java`-launcher positional semantics: every token *after* the
/// program selector is a program argument and must be passed to the Java
/// application verbatim -- even if it starts with `-`/`--` or equals
/// `--help` / `--version` / `--list-modules`.
///
/// The program is selected by either `-jar <jarfile>` or the first bare
/// (non-option) token used as a main-class name. This function scans the
/// leading option section and, as soon as it identifies the selector,
/// inserts a literal `--` separator immediately after it. The downstream
/// stages (`normalize_java_launcher_argv`, `extract_system_properties`,
/// `extract_hotspot_flags`) and clap itself all treat everything past `--`
/// as opaque program args, so launcher options are recognised only in the
/// leading section -- matching the stock `java` launcher.
///
/// If the caller already supplied an explicit `--`, or no program selector
/// is present (e.g. `java --version` / `java --help` with no program), the
/// argv is returned unchanged so the launcher still handles those itself.
/// The JDK `java` launcher's module launch, `-m <module>[/<mainclass>]`
/// (`--module <..>`, `--module=<..>`), in this launcher's terms: the module is
/// added as a root (`--add-modules`, which `resolve_module_path` resolves from
/// `-p` / `--module-path` at VM init and puts on the application search path)
/// and the main class becomes the program selector, so every token after the
/// module operand stays a program argument. With no `/<mainclass>` the class
/// is the module descriptor's `ModuleMainClass`.
///
/// `Err` is the text HotSpot prints, with exit status 1 (the boot-layer
/// abort on stdout, the launcher's texts on stderr; see the caller):
///
/// * a module observable neither on the module path nor in the JDK image, or
///   a module it requires (not `static`) that is missing:
///   `Error occurred during initialization of boot layer` /
///   `java.lang.module.FindException: Module <m> not found[, required by <r>]`
///   (the VM's boot-layer abort);
/// * no main class: `module <m> does not have a ModuleMainClass attribute,
///   use -m <module>/<main-class>` (`LauncherHelper`,
///   `java.launcher.module.error1`).
///
/// argv is returned unchanged when there is no module launch: a `--`, `-jar`
/// or a bare main class first. Interpreter round i4 wave 86, lane L5 (probe
/// `L5W86ModuleLaunch`): the launcher had no `-m` at all (clap refused it).
/// Is `name` a module of the JDK image at `java_home`:
/// `<java.home>/jmods/<m>.jmod`, or the image's module list (`release`'s
/// `MODULES` line)? `None` when neither can be read (no refusal is then
/// made on this evidence).
pub(crate) fn jdk_image_has_module(java_home: Option<&str>, name: &str) -> Option<bool> {
    let home = std::path::PathBuf::from(java_home?);
    let jmods = home.join("jmods");
    if jmods.is_dir() {
        return Some(jmods.join(format!("{name}.jmod")).is_file());
    }
    let release = std::fs::read_to_string(home.join("release")).ok()?;
    let line = release.lines().find(|l| l.starts_with("MODULES="))?;
    let list = line["MODULES=".len()..].trim_matches('"');
    Some(list.split_whitespace().any(|m| m == name))
}

pub(crate) fn rewrite_module_launch(args: Vec<String>) -> Result<Vec<String>, String> {
    let mut module_path: Vec<String> = Vec::new();
    let mut i = 1usize;
    let (at, spec, rest_from) = loop {
        let Some(a) = args.get(i).map(String::as_str) else {
            return Ok(args);
        };
        if a == "--" || a == "-jar" || a == "--jar" || a.starts_with("-jar=") || a.starts_with("--jar=")
        {
            return Ok(args);
        }
        if a == "-p" || a == "--module-path" {
            if let Some(v) = args.get(i + 1) {
                module_path.push(v.clone());
            }
            i += 2;
            continue;
        }
        if let Some(v) = a.strip_prefix("--module-path=") {
            module_path.push(v.to_string());
            i += 1;
            continue;
        }
        if a == "-m" || a == "--module" {
            let Some(spec) = args.get(i + 1) else {
                return Ok(args);
            };
            break (i, spec.clone(), i + 2);
        }
        if let Some(spec) = a.strip_prefix("--module=") {
            break (i, spec.to_string(), i + 1);
        }
        if VALUE_TAKING_OPTS.contains(&a) {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        return Ok(args);
    };
    let (module, class) = match spec.split_once('/') {
        Some((m, c)) => (m.to_string(), Some(c.to_string())),
        None => (spec.clone(), None),
    };
    let entries: Vec<String> = module_path
        .iter()
        .flat_map(|p| {
            std::env::split_paths(p)
                .map(|x| x.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .filter(|e| !e.is_empty())
        .collect();
    // A module path the boot layer refuses while scanning (wave 103, lane
    // L5): reported before any `Module <m> not found`, as `ModulePath` scans
    // its entries before the resolver can miss a module.
    // Multi-release jars at the booted version (wave 105, lane L5): the VM
    // options are the tokens before `-m`.
    // zt t16/tg7 (`CRATONVM_LAUNCHER_PATCH_MODULE_REFUSALS`): the boot
    // layer decodes `--patch-module` (`ModuleBootstrap`'s static
    // initializer) before it scans the module path.
    if launcher_patch_module_refusals_enabled() {
        let options = scan_boot_layer_module_options(&args);
        if let Some(PatchModuleRefusal::BootLayer(message)) =
            patch_module_refusal(&options.patch_module)
        {
            return Err(format!(
                "Error occurred during initialization of boot layer\n{}",
                boot_layer_patch_exception(&message)
            ));
        }
    }
    let java_home = launcher_java_home(scan_explicit_java_home(&args));
    let vm_opts = &args[..at];
    let booted = (!matches!(
        scan_requested_jdk_mode(vm_opts),
        cratonvm_vm::config::JdkMode::Synthetic
    ))
    .then(|| detect_jdk_feature(java_home.as_deref()))
    .flatten();
    let feature = launch_jar_runtime_feature(
        booted,
        argv_system_property(vm_opts, "jdk.util.jar.version"),
        argv_system_property(vm_opts, "jdk.util.jar.enableMultiRelease"),
    );
    if let Some(text) = cratonvm_classloading::module::module_path_find_error_at(&entries, feature)
    {
        return Err(format!(
            "Error occurred during initialization of boot layer\n\
             java.lang.module.FindException: {text}"
        ));
    }
    let observable = cratonvm_classloading::module::scan_module_path_at(&entries, feature);
    let system_module = |name: &str| jdk_image_has_module(java_home.as_deref(), name);
    let observable_named = |name: &str| observable.iter().find(|m| m.descriptor.name == name);
    let not_found = |name: &str, required_by: Option<&str>| {
        let tail = required_by
            .map(|r| format!(", required by {r}"))
            .unwrap_or_default();
        format!(
            "Error occurred during initialization of boot layer\n\
             java.lang.module.FindException: Module {name} not found{tail}"
        )
    };
    let root = observable_named(&module);
    if root.is_none() && system_module(&module) == Some(false) {
        return Err(not_found(&module, None));
    }
    // The module-path closure of `requires` (a JDK module's own requires are
    // the image's business).
    if let Some(root) = root {
        let mut seen: Vec<String> = vec![root.descriptor.name.clone()];
        let mut work = vec![root];
        while let Some(m) = work.pop() {
            for req in m.descriptor.requires.iter().filter(|r| !r.is_static) {
                if seen.contains(&req.module_name) {
                    continue;
                }
                seen.push(req.module_name.clone());
                match observable_named(&req.module_name) {
                    Some(dep) => work.push(dep),
                    None => {
                        if system_module(&req.module_name) == Some(false) {
                            return Err(not_found(&req.module_name, Some(&m.descriptor.name)));
                        }
                    }
                }
            }
        }
    }
    let main_class = match class {
        Some(c) => c,
        None => match root.and_then(|m| m.descriptor.main_class.clone()) {
            Some(c) => c.replace('/', "."),
            None => {
                return Err(format!(
                    "module {module} does not have a ModuleMainClass attribute, use -m \
                     <module>/<main-class>"
                ))
            }
        },
    };
    // `LauncherHelper.loadModuleMainClass`: `Class.forName(module, name)`
    // answering null is `java.launcher.module.error2`. Decidable here for a
    // module-path module: the class's package must be one of the module's,
    // and an exploded module's directory must hold the class file (a modular
    // jar is left to the VM).
    if let Some(root) = root {
        let internal = main_class.replace('.', "/");
        let package = internal.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let root_dir = std::path::Path::new(&root.root);
        let missing = (!root.packages.is_empty() && !root.packages.iter().any(|p| p == package))
            || (root_dir.is_dir() && !root_dir.join(format!("{internal}.class")).is_file());
        if missing {
            return Err(format!(
                "Error: Could not find or load main class {main_class} in module {module}"
            ));
        }
    }
    let mut out: Vec<String> = args[..at].to_vec();
    // `java.c` names the main module to the VM as `-Djdk.module.main=<m>`
    // (visible to the program, as HotSpot's is), and the VM's launch hands
    // `<m>/<class>` to `LauncherHelper.checkAndLoadMain` with `LM_MODULE` when
    // it sees the property (`Vm::launcher_check_main_via_helper`; interpreter
    // round i6 wave 117, lane L5).
    out.push(format!("-Djdk.module.main={module}"));
    out.push("--add-modules".to_string());
    out.push(module);
    out.push(main_class);
    out.push("--".to_string());
    out.extend_from_slice(&args[rest_from.min(args.len())..]);
    Ok(out)
}

/// zt t16/tg7: how [`scan_launcher_missing_value_refusal_with`] reads `-jar`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JarScanRules {
    /// A `-jar` with nothing after it is libjli's `ARG_CHECK` refusal, with
    /// the full usage text ([`bare_jar_refusal_text`];
    /// `CRATONVM_LAUNCHER_USAGE_TEXT`).
    pub(crate) bare_jar_usage: bool,
    /// The scan goes on past `-jar`: libjli's `-jar` sets the launch mode and
    /// takes no value; the jar is the first token that is not an option
    /// (`CRATONVM_LAUNCHER_JAR_OPTION_SCAN`, [`relocate_jar_selector`]).
    pub(crate) scan_past_jar: bool,
}

/// `scan_launcher_missing_value_refusal` with the zt t16/tg7 rules for
/// `-jar` ([`JarScanRules`]); `missing_values` false makes no "requires"
/// refusal (the `CRATONVM_LAUNCHER_MISSING_VALUE_REFUSALS=0` kill) but
/// walks the same tokens, so a bare `-jar` is still found. libjli parses in
/// command-line order: `--add-modules -jar` names `--add-modules` (its value
/// may not start with `-`), `-jar --add-modules` names `--add-modules` too
/// (`-jar` had a token after it), `-cp x -jar` is the bare `-jar`. Pure.
pub(crate) fn scan_launcher_missing_value_refusal_with(
    argv: &[String],
    missing_values: bool,
    jar: JarScanRules,
) -> Option<String> {
    const MODULES: [&str; 7] = [
        "--add-modules",
        "--add-exports",
        "--add-opens",
        "--add-reads",
        "--patch-module",
        "--limit-modules",
        "--enable-native-access",
    ];
    const MODULE_PATHS: [&str; 3] = ["--module-path", "-p", "--upgrade-module-path"];
    const CLASS_PATHS: [&str; 3] = ["-cp", "-classpath", "--class-path"];
    const LONG_FORMS: [&str; 8] = [
        "--module-path=",
        "--upgrade-module-path=",
        "--add-modules=",
        "--enable-native-access=",
        "--limit-modules=",
        "--add-exports=",
        "--add-reads=",
        "--patch-module=",
    ];
    let mut i = 1usize;
    while let Some(arg) = argv.get(i).map(String::as_str) {
        if LAUNCHER_SCAN_RETURNS.contains(&arg) || !arg.starts_with('-') {
            return None;
        }
        if arg == "-jar"
            || arg == "--jar"
            || arg.starts_with("-jar=")
            || arg.starts_with("--jar=")
        {
            if arg == "-jar" {
                // zt t16/tg7: libjli's `ARG_CHECK(argc, ARG_ERROR2, arg)`
                // counts the tokens after `-jar`.
                if jar.bare_jar_usage && i + 1 == argv.len() {
                    return Some(bare_jar_refusal_text());
                }
                if jar.scan_past_jar && i + 1 < argv.len() {
                    i += 1;
                    continue;
                }
            }
            return None;
        }
        // `IsOptionWithArgument`: the next token is a value only when it does
        // not start with `-`. zt t14/tg6: libjli's `has_arg` also wants it
        // non-empty (`--add-opens ""` is a missing value); only the class
        // path takes an empty one (`has_arg_any_len`).
        let next = argv.get(i + 1).filter(|next| !next.starts_with('-'));
        let has_value = next.is_some_and(|next| !next.is_empty());
        let requires = if MODULES.contains(&arg) {
            Some("modules to be specified")
        } else if MODULE_PATHS.contains(&arg) {
            Some("module path specification")
        } else if CLASS_PATHS.contains(&arg) {
            Some("class path specification")
        } else if arg == "-m" || arg == "--module" {
            if has_value || !missing_values {
                // The module is the program selector: the rest is the
                // application's.
                return None;
            }
            Some("module name")
        } else {
            None
        };
        if let Some(what) = requires {
            let class_path = CLASS_PATHS.contains(&arg);
            if !(has_value || (class_path && next.is_some())) {
                if missing_values {
                    return Some(format!("Error: {arg} requires {what}"));
                }
                i += if next.is_some() { 2 } else { 1 };
                continue;
            }
            i += 2;
            continue;
        }
        if arg == "--module=" && missing_values {
            return Some("Error: --module= requires module name".to_string());
        }
        if arg.starts_with("--module=") {
            return None;
        }
        if arg == "--source" {
            if !has_value {
                if missing_values {
                    return Some("Error: --source requires source version".to_string());
                }
                i += if next.is_some() { 2 } else { 1 };
                continue;
            }
            i += 2;
            continue;
        }
        if arg == "--source=" && missing_values {
            return Some("Error: --source= requires source version".to_string());
        }
        if arg.starts_with("--source=") {
            i += 1;
            continue;
        }
        if missing_values && LONG_FORMS.contains(&arg) {
            return Some(format!("Error: {arg} requires modules to be specified"));
        }
        i += if VALUE_TAKING_OPTS.contains(&arg) {
            2
        } else {
            1
        };
    }
    None
}

/// The tokens at which libjli's option parse returns (the version and help
/// queries, `-X`, `--`): options after them never reach the VM
/// (`-version --add-modules` prints the banner, rc 0).
pub(crate) const LAUNCHER_SCAN_RETURNS: [&str; 11] = [
    "-version",
    "--version",
    "-fullversion",
    "--full-version",
    "-help",
    "--help",
    "-h",
    "-?",
    "--help-extra",
    "-X",
    "--",
];

/// HotSpot 25 refuses `-jar` and `-m` / `--module` / `--module=...` when
/// `--source` is selected (`Error: Option <opt> is not allowed with --source`).
pub(crate) fn scan_source_launcher_conflict_refusal(argv: &[String]) -> Option<String> {
    let mut i = 1usize;
    let mut seen_source = false;
    while let Some(arg) = argv.get(i).map(String::as_str) {
        if arg == "--" || LAUNCHER_SCAN_RETURNS.contains(&arg) {
            return None;
        }
        if !arg.starts_with('-') {
            return None;
        }
        if arg == "--source" {
            seen_source = true;
            let next = argv.get(i + 1).filter(|next| !next.starts_with('-'));
            if next.is_some() {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if arg.starts_with("--source=") {
            seen_source = true;
            i += 1;
            continue;
        }
        if seen_source {
            if arg == "-jar" || arg == "--jar" {
                return Some("Error: Option -jar is not allowed with --source".to_string());
            }
            if arg == "-m" {
                return Some("Error: Option -m is not allowed with --source".to_string());
            }
            if arg == "--module" {
                return Some("Error: Option --module is not allowed with --source".to_string());
            }
            if arg.starts_with("--module=") {
                return Some(format!("Error: Option {arg} is not allowed with --source"));
            }
        }
        let next_is_value = argv.get(i + 1).is_some_and(|next| !next.starts_with('-'));
        i += if VALUE_TAKING_OPTS.contains(&arg) {
            if next_is_value {
                2
            } else {
                1
            }
        } else {
            1
        };
    }
    None
}



/// zt t14/tg6: the `--add-reads` / `--add-exports` / `--add-opens` values
/// of the leading option section, each in command-line order
/// ([`scan_boot_layer_module_options`]). zt t16/tg7: also the
/// `--patch-module` values ([`patch_module_refusal`]) and the module path
/// (`-p` / `--module-path`, the last one, as HotSpot's `jdk.module.path`;
/// [`launcher_module_path_find_error`]).
#[derive(Debug, Default, PartialEq)]
pub(crate) struct BootLayerModuleOptions {
    pub(crate) add_reads: Vec<String>,
    pub(crate) add_exports: Vec<String>,
    pub(crate) add_opens: Vec<String>,
    pub(crate) patch_module: Vec<String>,
    pub(crate) module_path: Option<String>,
}

/// zt t14/tg6: the boot-layer options of the leading section, read from raw
/// argv for the version path (which exits before clap). The section ends as
/// in [`scan_launcher_missing_value_refusal_with`]: at a version or help query,
/// `-X`, `--`, the program selector (a bare main class, `-jar`, `-m`). A
/// separate value is the next token when it does not start with `-`. Pure.
pub(crate) fn scan_boot_layer_module_options(argv: &[String]) -> BootLayerModuleOptions {
    let mut out = BootLayerModuleOptions::default();
    let mut module_paths: Vec<String> = Vec::new();
    let mut i = 1usize;
    while let Some(arg) = argv.get(i).map(String::as_str) {
        if LAUNCHER_SCAN_RETURNS.contains(&arg) || !arg.starts_with('-') {
            break;
        }
        if arg == "-jar"
            || arg == "--jar"
            || arg.starts_with("-jar=")
            || arg.starts_with("--jar=")
            || arg == "-m"
            || arg == "--module"
            || arg.starts_with("--module=")
        {
            break;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value)),
            _ => (arg, None),
        };
        let list = match name {
            "--add-reads" => Some(&mut out.add_reads),
            "--add-exports" => Some(&mut out.add_exports),
            "--add-opens" => Some(&mut out.add_opens),
            "--patch-module" => Some(&mut out.patch_module),
            "-p" | "--module-path" => Some(&mut module_paths),
            _ => None,
        };
        if let Some(list) = list {
            match inline {
                Some(value) => {
                    list.push(value.to_string());
                    i += 1;
                }
                None => match argv.get(i + 1) {
                    Some(value) if !value.starts_with('-') => {
                        list.push(value.clone());
                        i += 2;
                    }
                    _ => i += 1,
                },
            }
            continue;
        }
        i += if VALUE_TAKING_OPTS.contains(&arg) {
            2
        } else {
            1
        };
    }
    out.module_path = module_paths.pop();
    out
}

