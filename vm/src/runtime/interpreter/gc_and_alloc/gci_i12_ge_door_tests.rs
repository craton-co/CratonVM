//! gci i12/ge: the JIT array helpers' humongous door (i11/go's narrowed
//! JIT half of `gci-i10rh-...-review-findings-20261013` finding 4), the
//! one-report scope's wiring in `force_full_collection`
//! (`gci-i10dl-...-door-residuals-20261013` (b)) and the G1 remark's
//! reference-block id (`gci-i11go-...-review-findings-20261013` item 1).
use super::*;
use crate::config::{GcAlgorithm, VmConfig};


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

/// A G1 humongous request through the JIT array helpers collects on
/// `g1-humongous-alloc` (`G1HumongousAllocation`); every other request
/// and backend keeps `jit-helpers`. Both failure arms use it.
#[test]
fn gci_i12_ge_the_jit_array_doors_report_a_humongous_request() {
    assert_eq!(jit_array_site_for(G1_HUMONGOUS_ALLOC_SITE), G1_HUMONGOUS_ALLOC_SITE);
    assert_eq!(jit_array_site_for("gc-alloc-array"), "jit-helpers");
    assert_eq!(
        forced_site_door(jit_array_site_for(G1_HUMONGOUS_ALLOC_SITE), false, None),
        GcDoor::G1HumongousAllocation
    );
    assert_eq!(
        forced_site_door(jit_array_site_for("gc-alloc-array"), false, None),
        GcDoor::AllocationFailure
    );
    let generational = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    });
    assert_eq!(jit_array_gc_site(&generational, usize::MAX / 2), "jit-helpers");
    let g1 = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::G1,
        ..VmConfig::default()
    });
    let half = match &g1.mem.heap {
        _ if g1.mem.heap.is_g1() => g1.mem.heap.region_census().map(|c| c.region_size).unwrap_or(0) / 2,
        _ => unreachable!("a G1 VmConfig builds a G1 heap"),
    };
    assert_eq!(jit_array_gc_site(&g1, half), "jit-helpers");
    assert_eq!(jit_array_gc_site(&g1, half + 1), G1_HUMONGOUS_ALLOC_SITE);
    // The split `vm/src/jit/helpers/*.rs`, concatenated (origin/dev `00529e979f`).
    let helpers = crate::jit::helpers::read_helpers_src_from_disk().replace("\r\n", "\n");
    // gci i21/fx: three since gci i20/ys's YS-H1 hunk -- the newarray and
    // anewarray failure arms, plus `jit_newarray`'s humongous
    // concurrent-start arm (`humongous_alloc_wants_concurrent_start`),
    // whose collection is a humongous request's too.
    assert_eq!(
        helpers
            .matches("crate::runtime::interpreter::gc_and_alloc::jit_array_gc_site(vm, total_size),")
            .count(),
        3,
        "the newarray and anewarray failure arms and the newarray humongous \
         concurrent-start arm"
    );
    assert_eq!(
        helpers
            .matches("if heap.g1_humongous_alloc_wants_concurrent_start(total_size) \u{7b}")
            .count(),
        1,
        "the one concurrent-start arm the third door belongs to"
    );
}

/// `force_full_collection` opens the report before its first pause,
/// ends the folding of marking pauses once its own cycle has finished
/// (before the compaction pause and its mark drive), and closes it last.
#[test]
fn gci_i12_ge_the_full_collection_brackets_its_report() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let f = body(&src, "fn force_full_collection(");
    let begin = f
        .find("super::gc_events::g1_full_report_begin(shared, door)")
        .expect("opened");
    let first = f.find("for _attempt in 0..FORCE_GC_ATTEMPTS").expect("the loop");
    let cycle = f.find("g1_force_full_cycle(shared, thread);").expect("the cycle");
    let done = f
        .find("super::gc_events::g1_full_report_marking_done(full_report.as_ref())")
        .expect("marking done");
    let compaction = f
        .find("g1_full_compaction_pause(shared, thread, door);")
        .expect("the compaction");
    let finish = f.find("report.finish()").expect("closed");
    assert!(begin < first && first < cycle && cycle < done && done < compaction);
    assert!(compaction < finish);
    // The remark's reference block files under the cycle's id on G1.
    let id = body(&src, "fn remark_reference_xlog_id(");
    assert!(id.contains("super::gc_events::g1_open_cycle_id(shared)"));
}

/// Findings page item 6: a humongous request's G1 headroom guard is
/// `G1HumongousAllocation` and still an OCCUPANCY door; other requests
/// keep the guard's own site.
#[test]
fn gci_i12_ge_the_headroom_guards_report_a_humongous_request() {
    for ordinary in ["jit-newarray-headroom", "jit-anewarray-headroom"] {
        assert_eq!(
            jit_headroom_site_for(G1_HUMONGOUS_ALLOC_SITE, ordinary),
            G1_HUMONGOUS_HEADROOM_SITE
        );
        assert_eq!(jit_headroom_site_for("gc-alloc-array", ordinary), ordinary);
    }
    assert!(occupancy_door_site(G1_HUMONGOUS_HEADROOM_SITE));
    assert_eq!(
        forced_site_door(G1_HUMONGOUS_HEADROOM_SITE, true, None),
        GcDoor::G1HumongousAllocation
    );
    assert_eq!(
        forced_site_door(G1_HUMONGOUS_HEADROOM_SITE, true, Some(GcDoor::SystemGc)),
        GcDoor::SystemGc
    );
    let g1 = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::G1,
        ..VmConfig::default()
    });
    let half = match &g1.mem.heap {
        _ if g1.mem.heap.is_g1() => g1.mem.heap.region_census().map(|c| c.region_size).unwrap_or(0) / 2,
        _ => unreachable!("a G1 VmConfig builds a G1 heap"),
    };
    assert_eq!(
        jit_array_headroom_site(&g1, half + 1, "jit-newarray-headroom"),
        G1_HUMONGOUS_HEADROOM_SITE
    );
    assert_eq!(
        jit_array_headroom_site(&g1, half, "jit-newarray-headroom"),
        "jit-newarray-headroom"
    );
    // The split `vm/src/jit/helpers/*.rs`, concatenated (origin/dev `00529e979f`).
    let helpers = crate::jit::helpers::read_helpers_src_from_disk().replace("\r\n", "\n");
    assert_eq!(
        helpers
            .matches("crate::runtime::interpreter::gc_and_alloc::jit_array_headroom_site(")
            .count(),
        2
    );
}

/// Findings page item 1: the backend's `jdk.SystemGC` is written once
/// per explicit request, after its collection and before the drains.
#[test]
fn gci_i12_ge_the_backend_system_gc_row_is_per_request() {
    let src = super::GC_AND_ALLOC_SRC.replace("\r\n", "\n");
    let door = body(&src, "fn full_gc_door(");
    let collect = door
        .find("force_full_collection(shared, thread, GcDoor::SystemGc)")
        .expect("the collection");
    let row = door
        .find("cratonvm_native_builtins_jfr::jfr_vm_more::system_gc_by(")
        .expect("the backend row");
    let drain = door.find("run_finalizers_forced(").expect("the drains");
    assert!(collect < row && row < drain);
    assert!(door[..row].contains("backend_system_gc = Some("));
}
