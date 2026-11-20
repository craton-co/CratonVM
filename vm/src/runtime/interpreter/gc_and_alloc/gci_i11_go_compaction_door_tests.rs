//! gci i11/go (`gci-i10dl-g1-full-collection-door-residuals-FIXED-20261016` (a)
//! and (c)): G1's allocation-failure recovery compaction runs on
//! `GcDoor::G1CompactionPause`, and a full collection's compaction pause
//! may skip the mark-cycle drive under the opt-in switch.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use cratonvm_gc::gen_heap::oome_ladder as l;
use std::sync::atomic::Ordering;

fn source() -> String {
    super::GC_AND_ALLOC_SRC.replace("\r\n", "\n")
}

/// The item starting at `sig` at the start of a line, up to the next
/// column-0 closing brace (`\u{7d}`, see `w5a_pause_driver_tests::body`).
fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

/// The recovery-compaction site without a requester is
/// `G1CompactionPause`; a requester still wins; every other site keeps
/// the i8/gs allocation door. The new door is an allocation door for the
/// overhead verdict, with HotSpot's `G1 Compaction Pause` cause.
#[test]
fn gci_i11_go_the_recovery_compaction_has_its_own_door() {
    for occupancy in [false, true] {
        assert_eq!(
            forced_site_door(G1_LAST_DITCH_COMPACTION_SITE, occupancy, None),
            GcDoor::G1CompactionPause
        );
        assert_eq!(
            forced_site_door(G1_LAST_DITCH_COMPACTION_SITE, occupancy, Some(GcDoor::SystemGc)),
            GcDoor::SystemGc
        );
    }
    for site in ["unlabelled", "vm-exec", "g1-full-compaction", "tlab-alloc-shaped"] {
        for occupancy in [false, true] {
            assert_eq!(
                forced_site_door(site, occupancy, None),
                forced_pause_door(occupancy, None),
                "{site}"
            );
        }
    }
    assert_eq!(G1_LAST_DITCH_COMPACTION_SITE, "g1-last-ditch-compaction");
    assert!(door_samples_productivity(GcDoor::G1CompactionPause));
    assert_eq!(GcDoor::G1CompactionPause.cause(), "G1 Compaction Pause");
}

/// Wiring: both recovery-compaction calls name the site constant (no
/// literal that could drift from `forced_site_door`'s), the forced door
/// picks through `forced_site_door`, the `System.gc()` coalescing bump
/// does not include the new door, and the mtroots reason is the forced
/// one.
#[test]
fn gci_i11_go_the_recovery_compaction_is_wired() {
    let src = source();
    let ditch = body(&src, "fn g1_last_ditch_recovery_pause(");
    assert_eq!(
        ditch
            .matches("maybe_gc_forced_at(shared, thread, G1_LAST_DITCH_COMPACTION_SITE)")
            .count(),
        // The recovery pause, its staging loop, the fragmentation slide.
        3
    );
    assert!(!ditch.contains("\"g1-last-ditch-compaction\""));
    // The forced door's body since the origin/dev merge (zr z3/ja's stamp).
    let on = body(&src, "fn maybe_gc_forced_collected_stamped_on(");
    assert!(on.contains("let door = forced_site_door(site, occupancy_door, requester);"));
    assert!(on.contains(
        "&& !full_compaction_skips_mark_drive(requester, g1_full_compaction_skips_mark_drive)"
    ));
    let pause = body(&src, "fn run_collection_pause(");
    assert!(pause.contains("GcDoor::AllocationFailure | GcDoor::G1CompactionPause => 3,"));
    let bump = pause
        .find("system_gc_collections(shared).fetch_add(")
        .expect("the coalescing bump");
    let bump_arm = &pause[pause[..bump].rfind("if matches!(").expect("its gate")..bump];
    assert!(!bump_arm.contains("G1CompactionPause"), "{bump_arm}");
}

/// (c): only a requester's pause asks the switch, and skips only when it
/// is on.
#[test]
fn gci_i11_go_only_a_requesters_pause_may_skip_the_mark_drive() {
    assert!(!full_compaction_skips_mark_drive(None, || panic!(
        "an allocation pause must not read the switch"
    )));
    assert!(full_compaction_skips_mark_drive(
        Some(GcDoor::SystemGc),
        || true
    ));
    assert!(!full_compaction_skips_mark_drive(
        Some(GcDoor::MetadataThreshold),
        || false
    ));
}

/// i10rh finding 4: a G1 humongous array request's collection runs on
/// `G1HumongousAllocation` (an allocation door in every other respect);
/// the site is chosen only on G1 and only above half a region.
#[test]
fn gci_i11_go_a_humongous_request_has_its_own_door() {
    for occupancy in [false, true] {
        assert_eq!(
            forced_site_door(G1_HUMONGOUS_ALLOC_SITE, occupancy, None),
            GcDoor::G1HumongousAllocation
        );
        assert_eq!(
            forced_site_door(G1_HUMONGOUS_ALLOC_SITE, occupancy, Some(GcDoor::SystemGc)),
            GcDoor::SystemGc
        );
    }
    assert!(door_samples_productivity(GcDoor::G1HumongousAllocation));
    assert_eq!(GcDoor::G1HumongousAllocation.cause(), "G1 Humongous Allocation");
    let generational = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    assert_eq!(gc_alloc_array_site(&generational, usize::MAX / 2), "gc-alloc-array");
    let g1 = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::G1,
        ..VmConfig::default()
    });
    let half = g1.mem.heap.region_census().map(|c| c.region_size).unwrap_or(0) / 2;
    assert_eq!(gc_alloc_array_site(&g1, half), "gc-alloc-array");
    assert_eq!(gc_alloc_array_site(&g1, half + 1), G1_HUMONGOUS_ALLOC_SITE);
    let src = source();
    let arr = body(&src, "pub(crate) fn gc_alloc_array_unsampled(");
    assert!(arr.contains("let site = gc_alloc_array_site(shared, external_bytes);"));
    assert!(arr.contains("collect_and_retry(shared, thread, site, |s| {"));
    let pause = body(&src, "fn run_collection_pause(");
    assert!(pause.contains("GcDoor::G1HumongousAllocation => 3,"));
}

/// R3 (`gci-i11rp-...-review-findings-20261013`): a remark round's census
/// counts what the round cleared / enqueued per kind (final with the
/// resurrected finalizables, phantom with the cleaners) and renders
/// HotSpot's `gc+ref` / `gc+phases+ref` block with the phases that ran.
#[test]
fn gci_i11_go_a_remark_round_has_a_reference_census() {
    let stats = cratonvm_gc::reference::ReferenceProcessingStats {
        soft_refs_cleared: 1,
        weak_refs_cleared: 4,
        finalizer_refs_enqueued: 0,
        finalizer_refs_resurrected: 2,
        phantom_refs_enqueued: 1,
        cleaner_refs_processed: 2,
        ..Default::default()
    };
    let c = remark_ref_census(&stats, 1_500_000);
    assert_eq!(c.discovered, [1, 4, 2, 3]);
    assert_eq!(c.processed, [1, 4, 2, 3]);
    assert_eq!(c.dropped, [0; 4]);
    assert_eq!(c.phase_ns, [1_500_000, 0, 0]);
    let lines: Vec<String> = crate::runtime::unified_logging::xlog_reference_lines(7, &c)
        .into_iter()
        .map(|(_, _, t)| t)
        .collect();
    assert!(lines.iter().all(|l| l.starts_with("GC(7) ")), "{lines:?}");
    assert!(!lines.iter().any(|l| l.contains("Skipped")), "{lines:?}");
    assert!(lines.contains(&"GC(7)   WeakReference Discovered: 4, Dropped: 0, Processed: 4".to_string()));
    // An empty round skips every phase, as HotSpot prints it.
    let none = remark_ref_census(&Default::default(), 0);
    let skipped = crate::runtime::unified_logging::xlog_reference_lines(1, &none)
        .into_iter()
        .filter(|(_, _, t)| t.contains("Skipped"))
        .count();
    assert_eq!(skipped, 3);
}

/// R3 wiring: the remark round defers its block under
/// `remark_reference_xlog_id`, and both remark doors flush it after
/// their pause line (after the release: no I/O inside the pause).
#[test]
fn gci_i11_go_both_remarks_flush_their_reference_block() {
    let src = source();
    let rows = body(&src, "fn remark_reference_rows(");
    let call = rows
        .find("process_references_with_finalizer_trace(")
        .expect("the round");
    let defer = rows
        .find("defer_xlog_reference_processing(")
        .expect("the census is deferred");
    assert!(call < defer);
    assert!(rows.contains("remark_reference_xlog_id(shared),"));
    for sig in [
        "fn gen_concurrent_remark_pause(",
        "pub(in crate::runtime::interpreter) fn g1_final_remark_cleanup(",
    ] {
        let b = body(&src, sig);
        let finish = b
            .find("non_collection_pause_finish(pause_timer);")
            .unwrap_or_else(|| panic!("{sig}: finish"));
        let flush = b
            .find("flush_remark_reference_lines(shared);")
            .unwrap_or_else(|| panic!("{sig}: flush"));
        let release = b.find("complete_gc(").unwrap_or_else(|| panic!("{sig}: release"));
        assert!(release < finish && finish < flush, "{sig}");
    }
}

/// Behaviour (a Generational VM; the door is backend-agnostic): the
/// recovery-compaction site is still a forced ladder rung and still not a
/// full-collection door.
#[test]
fn gci_i11_go_the_recovery_compaction_is_a_ladder_rung() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    let census = |s: &SharedVm| match &s.mem.heap {
        _ if shared.mem.heap.is_generational() => shared.mem.heap.gen_oome_ladder_census().unwrap(),
        _ => [0; l::LEN],
    };
    let full_count = |s: &SharedVm| system_gc_collections(s).load(Ordering::Acquire);
    let mut thread = JvmThread::new(ThreadId(0), "i11go-door");
    let before = full_count(&shared);
    assert!(maybe_gc_forced_collected(
        &shared,
        &mut thread,
        G1_LAST_DITCH_COMPACTION_SITE
    ));
    assert_eq!(full_count(&shared), before, "not a full-collection door");
    assert_eq!(census(&shared)[l::FORCED], 1, "a forced ladder rung");
}
