use std::cell::Cell;
thread_local! {
    static GC_HOOK_STARTS: Cell<u32> = const { Cell::new(0) };
    static GC_HOOK_FINISHES: Cell<u32> = const { Cell::new(0) };
}
fn gc_hook_start_cb() { GC_HOOK_STARTS.with(|c| c.set(c.get() + 1)); }
fn gc_hook_finish_cb() { GC_HOOK_FINISHES.with(|c| c.set(c.get() + 1)); }
use crate::gc_quiescence::incomplete_reason;
use crate::gc_metrics::{g1_cycle_kind, g1_degraded};
use crate::heap::{ref_element_degradation_count, ref_element_word_implausible};
use crate::collector::GarbageCollector;
use std::sync::atomic::Ordering;

fn ref_degrade_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn make_collector() -> crate::G1Collector {
    crate::G1Collector::new(crate::G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..Default::default()
    })
}

struct NoopMonitors;
impl crate::MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}
fn stw() -> crate::StopTheWorldToken {
    unsafe { crate::StopTheWorldToken::new() }
}
use crate::gc_metrics::*;
use crate::*;
use cratonvm_types::*;

/// The hooks fire for the collectors the VM actually RUNS.
///
/// The two tests above drive `collect` / `collect_with_finalizers`, the
/// Cheney collector of the vestigial semi-space `Heap` — and until
/// 2026-09-23 those two functions were the ONLY callers of
/// `fire_gc_start` / `fire_gc_finish` in the workspace. No `GcBackend`
/// selects that collector, so on Generational, G1 and ZGC a JVMTI agent
/// that enabled `GarbageCollectionStart` / `GarbageCollectionFinish`
/// received nothing at all, while both tests above stayed green. The
/// hooks now fire at `VmHeap`'s dispatcher, once per collection, on both
/// public entry points — which is what this pins, per backend.
#[test]
fn gc_hooks_fire_around_every_vm_heap_collection() {
    use crate::vm_heap::{GcBackend, VmHeap};
    install_gc_start_hook(gc_hook_start_cb);
    install_gc_finish_hook(gc_hook_finish_cb);

    struct NoMonitors;
    impl crate::collector::MonitorCleanup for NoMonitors {
        fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
    }

    let mut backends = vec![GcBackend::Generational, GcBackend::G1];
        backends.push(GcBackend::Zgc);
    for backend in backends {
        let heap = VmHeap::new(backend, 8 * 1024 * 1024);
        let obj = heap.alloc_object(cratonvm_types::ClassId::new(1), 1);
        let mut roots = vec![obj];
        // SAFETY: single-threaded test; this thread is the only mutator.
        let stw = unsafe { crate::collector::StopTheWorldToken::new() };

        let before_start = GC_HOOK_STARTS.with(Cell::get);
        let before_finish = GC_HOOK_FINISHES.with(Cell::get);
        let _ = heap.collect_garbage_with_finalizers(&stw, &mut roots, &[], &NoMonitors);
        assert_eq!(
            GC_HOOK_STARTS.with(Cell::get),
            before_start + 1,
            "{backend:?}: GarbageCollectionStart must fire once per VmHeap collection"
        );
        assert_eq!(
            GC_HOOK_FINISHES.with(Cell::get),
            before_finish + 1,
            "{backend:?}: GarbageCollectionFinish must fire once per VmHeap collection"
        );

        let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        assert_eq!(
            GC_HOOK_STARTS.with(Cell::get),
            before_start + 2,
            "{backend:?}"
        );
        assert_eq!(
            GC_HOOK_FINISHES.with(Cell::get),
            before_finish + 2,
            "{backend:?}"
        );
    }
}

/// ...but garbage that merely happens to have bit 63 set is NOT a colored
/// word, and must keep the old degrade even in a `--features zgc` build
/// running Generational or G1. `0x8D8D..` sets bit 63 and fails
/// `is_well_formed` (reserved bits set, several metadata bits set).
#[test]
fn stale_garbage_with_bit63_still_degrades_under_the_zgc_feature() {
    let _guard = ref_degrade_test_lock();
    let raw: u64 = 0x8D8D_8D8D_8D8D_8D8D;
    assert!(crate::zgc::vaddr::is_colored_word(raw));
    assert!(
        !crate::zgc::vaddr::is_well_formed(raw),
        "if this ever becomes well-formed the panic arm would fire on real garbage"
    );
    let before = ref_element_degradation_count();
    assert_eq!(ref_element_word_implausible(raw), Value::Object(None));
    assert_eq!(ref_element_degradation_count() - before, 1);
}

#[test]
fn g1_cycle_line_is_appended_to_the_collector_decision_report() {
    record_collector_decision(
        "g1",
        decision_reason::MOVING_BACKEND_ALWAYS_EVACUATES,
        incomplete_reason::NONE,
    );
    record_g1_cycle(
        g1_cycle_kind::YOUNG,
        3,
        0,
        1,
        5,
        g1_degraded::JNI_PINNED_REGIONS_EXCLUDED,
    );
    let text = collector_decision_report();
    assert!(text.contains("backend=g1"), "{text}");
    assert!(text.contains("young=MOVING"), "{text}");
    assert!(text.contains("[GC] g1 cycle #1"), "{text}");
    assert!(text.contains("jni-pinned-regions-excluded"), "{text}");
}

#[test]
fn g1_cycle_is_absent_until_a_g1_pause_records_one() {
    // A Generational-only run must not grow a G1 line: this is the check
    // that keeps the report honest about which backend produced it.
    assert!(last_g1_cycle().is_none());
    record_collector_decision(
        "generational",
        decision_reason::MOVING_NO_JIT_FRAMES,
        incomplete_reason::NONE,
    );
    let text = collector_decision_report();
    assert!(!text.contains("g1 cycle"), "{text}");
}

/// gen r4w2/youngpolicy (2026-09-23): the three situations that used to
/// share `nonmoving-conservative-jit-roots` are three histogram rows, and
/// the approximate `[GC] conservative-jit-root diverts:` breakdown that
/// stood in for the split is gone rather than duplicated.
#[test]
fn the_three_former_conservative_root_diverts_are_three_rows() {
    for (code, n) in [
        (decision_reason::NON_MOVING_CONSERVATIVE_JIT_ROOTS, 1u64),
        (
            decision_reason::NON_MOVING_UNREWRITABLE_CONSERVATIVE_JIT_ROOTS,
            4,
        ),
        (decision_reason::NON_MOVING_YOUNG_COMPACTION_DISABLED, 2),
    ] {
        for _ in 0..n {
            record_collector_decision("generational", code, incomplete_reason::NONE);
        }
    }
    let (moving, non_moving, skipped, rows) = decision_histogram();
    assert_eq!((moving, non_moving, skipped), (0, 7, 0));
    for (label, n) in [
        ("nonmoving-conservative-jit-roots", 1u64),
        ("nonmoving-unrewritable-conservative-jit-roots", 4),
        ("nonmoving-young-compaction-disabled", 2),
    ] {
        assert!(
            rows.iter().any(|(l, c)| *l == label && *c == n),
            "{label} must be its own row with {n} cycles: {rows:?}"
        );
    }
    let text = collector_decision_report();
    assert!(
        !text.contains("conservative-jit-root diverts"),
        "the approximate split line must not come back beside the real rows: {text}"
    );
}

#[test]
fn decision_report_names_the_fallback_reason() {
    // Fresh test thread: nothing recorded, and the report says so rather
    // than inventing a verdict.
    assert!(last_collector_decision().is_none());
    let empty = collector_decision_report();
    // 2026-09-20: the needle used to be "no collection has run yet", which
    // is what the line USED to claim. It is a false claim on a default
    // (ZGC) run, which records no decision however much it collects, so the
    // line now reports what it knows and denies the stronger reading. The
    // old phrase still appears inside that denial, so assert on the part
    // that carries the meaning rather than on a substring that survives
    // either wording.
    assert!(
        empty.contains("no collector decision has been recorded"),
        "the empty report must say what it actually knows: {empty}"
    );
    assert!(
        empty.contains("This does NOT mean no collection has run yet"),
        "the empty report must deny the reading that ends an investigation \
         early on the default backend: {empty}"
    );

    crate::gc_quiescence::begin_moving_young_coverage_cycle();
    crate::gc_quiescence::mark_moving_young_coverage_incomplete_because(
        incomplete_reason::UNPUBLISHED_FRAME_OOP,
    );
    record_collector_decision(
        "generational",
        decision_reason::NON_MOVING_COVERAGE_INCOMPLETE,
        crate::gc_quiescence::moving_young_incomplete_reason(),
    );

    let d = last_collector_decision().expect("a decision was just recorded");
    assert_eq!(d.sequence, 1);
    assert_eq!(d.backend, "generational");
    assert!(!d.young_moving, "a coverage fallback is a NON-moving cycle");
    assert_eq!(
        d.incomplete_reason,
        incomplete_reason::UNPUBLISHED_FRAME_OOP
    );

    let text = collector_decision_report();
    assert!(text.contains("NON-MOVING"), "{text}");
    assert!(text.contains("nonmoving-coverage-incomplete"), "{text}");
    assert!(
        text.contains("compiled-frame-oop-not-published"),
        "the report must name the ROOT SOURCE that reported incomplete \
         coverage, not merely that coverage was incomplete: {text}",
    );
}

#[test]
fn the_decision_report_carries_the_g1_guard_counters_with_no_collection() {
    // Fresh test thread — the decision record is thread-local, so this is
    // the "nothing has run" shape a real process has before its first
    // collection, and the one the old location could not report at all.
    assert!(last_collector_decision().is_none());
    let text = collector_decision_report();
    for needle in [
        "[GC] g1 evac_ref_rejected=",
        "evac_holder_rejected=",
        "evac_holder_clamped=",
        "source_walk_desync=",
        "[GC] g1 non_object_roots_skipped=",
        // LANE W4-B — the two halves that used to have no reader.
        "COPIED=",
        "REEVACUATED_AFTER_RETIRE=",
        "[GC] g1 young evacuation: parallel=",
        "workers_last=",
        "[GC] g1 implausible_legacy_headers=",
        "copy_shape_drift=",
        "[GC] g1 flat_walk_refused_array=",
        "kept_seed_rejected=",
    ] {
        assert!(
            text.contains(needle),
            "the decision report must carry `{needle}`: it is the only shutdown census emitted on BOTH exit arms. Report was: {text}"
        );
    }
}

/// The G1 guard counters must be in the report, and must be there when
/// NOTHING has collected.
///
/// They used to live in `G1Collector::print_gc_summary`, whose comments
/// called them "unconditional ... before the early return" so that a zero
/// could be cited as evidence. That function is reached only from
/// `vm-cli`'s normal-return teardown, and `JUnitCore` -- every Tomcat, H2
/// and Spring Boot workload in the suites -- ends in `System.exit`, which
/// never unwinds Rust frames. So the zero was never printed once. This
/// report is the census emitted on BOTH exit arms.
///
/// Asserted on the LINE PREFIXES, not on the values: the counters behind
/// them are process statics that every other test in this binary shares,
/// and a test that pinned their values would be asserting test ordering.
/// Presence is exactly the property that was broken.
/// t1-Q: the report is printed with `eprintln!`, so an empty line inside it
/// (or a trailing newline) is an empty stderr line, which a harness that
/// strips `[`-led diagnostics from merged output reads as an extra stdout
/// line. The evac-share block's trailing `\n` met the mark-pauses block's
/// leading one and did exactly that on every `CRATONVM_GC_STATS` run.
#[test]
fn the_decision_report_has_no_empty_line() {
    let text = collector_decision_report();
    assert!(
        !text.contains("\n\n") && !text.starts_with('\n') && !text.ends_with('\n'),
        "the decision report must not carry an empty line: {text:?}"
    );
}

/// A well-formed ZGC colored word is legitimate data that has not been
/// through the load barrier — nulling it is silent heap corruption
/// (`zgc-jit-load-barrier.md` J1), so the cold arm must fail loudly instead.
#[test]
#[should_panic(expected = "with no load barrier")]
fn well_formed_colored_word_panics_instead_of_degrading() {
    use crate::zgc::vaddr;
    // Tagged (bit 63), reserved bits 62-46 clear, exactly one metadata bit.
    let colored: u64 = vaddr::Z_COLORED_TAG | vaddr::Z_MARKED0 | 0x40;
    assert!(vaddr::is_well_formed(colored));
    assert!(!cratonvm_types::plausible_heap_pointer(colored));
    let _ = ref_element_word_implausible(colored);
}

/// gc-common w4-e, step 2 of `common-g-proposal-one-satb-gate`: the queue
/// holds ONE count of the compiled-code SATB pre-gate for exactly its
/// accepting window -- taken by `activate` (once, however often it is
/// called), given back by every deactivation form (once, however often
/// they are called). The process-wide count itself is shared with every
/// other test in this binary, so only a lower bound is asserted on it.
#[test]
fn the_queue_holds_the_jit_pre_gate_exactly_while_it_accepts_logs() {
    let q = SatbQueue::new();
    assert!(
        !q.holds_jit_gate(),
        "a new queue is inactive and holds nothing"
    );

    q.activate();
    assert!(q.holds_jit_gate());
    assert!(
        crate::gen_heap::jit_ref_store_armed_markers() >= 1,
        "an active queue must keep the process-wide gate armed"
    );
    q.activate();
    assert!(q.holds_jit_gate(), "re-activation keeps the one count");

    q.deactivate_and_discard();
    assert!(!q.holds_jit_gate());
    q.deactivate_and_discard();
    assert!(
        !q.holds_jit_gate(),
        "a second deactivation gives nothing back"
    );

    q.activate();
    let _ = q.deactivate_and_drain(&stw());
    assert!(!q.holds_jit_gate());

    q.activate();
    q.deactivate();
    assert!(!q.holds_jit_gate());

    // Dies ACTIVE: `Drop` gives the count back (not observable per-queue
    // afterwards; this exercises the path under the test harness).
    q.activate();
    assert!(q.holds_jit_gate());
    drop(q);
}


    /// The G1 cycle record must reach the shared decision report, so a
    /// `--verbose:gc` run under `-XX:+UseG1GC` states which collector produced
    /// the summary and what it was unable to do.
    #[test]
    pub fn a_g1_pause_states_its_own_decision_in_the_report() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let mut roots = vec![obj];
        gc.collect_garbage(&stw(), &mut roots, &NoopMonitors);

        let text = crate::collector_decision_report();
        assert!(text.contains("backend=g1"), "{text}");
        assert!(text.contains("young=MOVING"), "{text}");
        assert!(text.contains("[GC] g1 cycle"), "{text}");
        assert!(text.contains("kind=young"), "{text}");
    }

    /// The evacuator census reaches the report with the VALUE the collector
    /// put in it, not a constant.
    ///
    /// `the_decision_report_carries_the_g1_guard_counters_with_no_collection`
    /// asserts the lines are present; this asserts one of them is wired to the
    /// counter behind it. `G1_YOUNG_PARALLEL` / `G1_YOUNG_SERIAL` are process
    /// statics that only go up, and a pause bumps exactly one of them, so
    /// `parallel + serial` after a real collection is non-zero however the
    /// rest of this binary's tests are scheduled -- which is the only
    /// assertion about a shared static that is not really an assertion about
    /// test ordering.
    #[test]
    pub fn the_report_carries_the_evacuator_census_the_collector_recorded() {
        let gc = make_collector();
        let obj = gc.alloc_object(ClassId::new(1), 1);
        let mut roots = vec![obj];
        gc.collect_garbage(&stw(), &mut roots, &NoopMonitors);

        let text = crate::collector_decision_report();
        let line = text
            .lines()
            .find(|l| l.starts_with("[GC] g1 young evacuation:"))
            .unwrap_or_else(|| panic!("no evacuator census in the report: {text}"));
        let field = |key: &str| -> usize {
            let rest = line
                .split_once(key)
                .unwrap_or_else(|| panic!("`{key}` missing from `{line}`"))
                .1;
            rest.split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("`{key}` is not a number in `{line}`"))
        };
        assert!(
            field("parallel=") + field("serial=") > 0,
            "a pause ran, so one of the two evacuator counters must have moved: `{line}`"
        );
    }

