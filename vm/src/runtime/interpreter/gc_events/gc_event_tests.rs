    use super::*;

    /// gen r4w2/obs: the JFR phase rows are consecutive and, with `other`,
    /// sum to the collector's pause.
    #[test]
    fn phase_pause_rows_partition_the_pause() {
        let rows = phase_pause_rows(1_000, 900, &[("roots", 100), ("cheney_drain", 650)]);
        assert_eq!(
            rows,
            vec![
                ("roots", 1_000, 100_000),
                ("cheney_drain", 101_000, 650_000),
                ("other", 751_000, 150_000),
            ]
        );
        let total: u64 = rows.iter().map(|r| r.2).sum();
        assert_eq!(total, 900_000);
        // No marks: one `other` row carrying the whole pause.
        assert_eq!(phase_pause_rows(5, 3, &[]), vec![("other", 5, 3_000)]);
    }

    /// gci i9/jc: the level-1 phase rows nest inside their collection's
    /// window: rows inside are unchanged, a straddling row is cut, rows
    /// outside are dropped, and an escalated pause's two halves partition the
    /// rows at the mark.
    #[test]
    fn gci_i9_jc_phase_rows_nest_inside_the_collection_window() {
        let rows = phase_pause_rows(1_000, 900, &[("roots", 100), ("cheney_drain", 650)]);
        // The whole pause's window: unchanged.
        assert_eq!(phase_rows_within(&rows, 1_000, 901_000), rows);
        // A shorter door window: the `other` row is cut at its end.
        assert_eq!(
            phase_rows_within(&rows, 1_000, 800_000),
            vec![
                ("roots", 1_000, 100_000),
                ("cheney_drain", 101_000, 650_000),
                ("other", 751_000, 49_000),
            ]
        );
        // An escalated pause split at 400_000: the halves partition the rows.
        let young = phase_rows_within(&rows, 1_000, 400_000);
        let full = phase_rows_within(&rows, 400_000, 901_000);
        assert_eq!(
            young,
            vec![("roots", 1_000, 100_000), ("cheney_drain", 101_000, 299_000)]
        );
        assert_eq!(
            full,
            vec![("cheney_drain", 400_000, 351_000), ("other", 751_000, 150_000)]
        );
        let total: u64 = young.iter().chain(full.iter()).map(|r| r.2).sum();
        assert_eq!(total, 900_000);
        // A zero-length row is kept only when its instant is in the window.
        let zero = [("other", 500, 0)];
        assert_eq!(phase_rows_within(&zero, 100, 500), vec![("other", 500, 0)]);
        assert!(phase_rows_within(&zero, 501, 900).is_empty());
        // No rows, no rows.
        assert!(phase_rows_within(&[], 0, 10).is_empty());
    }

    /// gci i9/jc (`gci-i8jf-jfr-cycle-rows-never-reach-the-jdk-jfr-backend`):
    /// a concurrent-cycle row is armed when EITHER recorder runs (the legacy
    /// one or the `jdk.jfr` backend), and not at all (no id taken) when
    /// neither runs.
    #[test]
    fn gci_i9_jc_cycle_rows_go_to_either_recorder() {
        assert_eq!(cycle_jfr_sinks(false, false), None);
        assert_eq!(cycle_jfr_sinks(true, false), Some((true, false)));
        assert_eq!(cycle_jfr_sinks(false, true), Some((false, true)));
        assert_eq!(cycle_jfr_sinks(true, true), Some((true, true)));
    }

    // gen r4w2/obs: `gc_event_heap_used` is deliberately NOT tested here with a
    // real heap. Constructing a `GenerationalHeap` publishes into process-global
    // bounds tables (`gc/tests/published_bounds_isolation.rs` explains the
    // flake that caused), and this binary runs whole VMs in parallel. The
    // computation -- a non-moving young sweep moves `VmHeap::live_bytes_estimate`
    // and leaves `allocated_bytes` where it was, which is the `8M->8M` bug --
    // is pinned in its own process by
    // `gc/tests/gengc_r4w2_obs.rs::xlog_occupancy_sees_a_non_moving_young_sweep`.

    /// gcd d10/o retirement: HotSpot 25 ZGC's start and end lines
    /// (25.0.4, `-XX:+UseZGC -Xmx256m -Xlog:gc`, `Gcd1MxNamesProbe`):
    /// `GC(0) Major Collection (System.gc()) 8M(3%)->2M(1%) 0.021s`.
    #[test]
    fn xlog_zgc_lines_have_hotspot_25s_shape() {
        const M: usize = 1024 * 1024;
        let [start, end] = render_xlog_zgc_lines(
            0,
            ZGC_MAJOR_COLLECTION,
            GcDoor::SystemGc.cause(),
            8 * M + 5,
            2 * M,
            256 * M,
            21_400_000,
        );
        assert_eq!(start, "GC(0) Major Collection (System.gc())");
        assert_eq!(
            end,
            "GC(0) Major Collection (System.gc()) 8M(3%)->2M(1%) 0.021s"
        );
        let [_, end] =
            render_xlog_zgc_lines(7, ZGC_MINOR_COLLECTION, "Allocation Rate", 0, 0, 0, 0);
        assert_eq!(
            end,
            "GC(7) Minor Collection (Allocation Rate) 0M(0%)->0M(0%) 0.000s"
        );
    }

    /// gcd d10/o retirement: a collection is Minor only when the young-only
    /// cycle count moved across it.
    #[test]
    fn collection_kind_follows_the_young_only_cycle_count() {
        use cratonvm_gc::gc_metrics::CollectionKind;
        assert_eq!(collection_kind(0, 0), CollectionKind::Major);
        assert_eq!(collection_kind(3, 3), CollectionKind::Major);
        assert_eq!(collection_kind(3, 4), CollectionKind::Minor);
    }

    #[test]
    fn xlog_gc_line_has_hotspots_shape() {
        let line = render_xlog_gc_line(
            3,
            "Pause Young",
            GcDoor::AllocationThreshold.cause(),
            24 * 1024 * 1024 + 5,
            3 * 1024 * 1024,
            96 * 1024 * 1024,
            2_345_000,
        );
        assert_eq!(
            line,
            "GC(3) Pause Young (Allocation Failure) 24M->3M(96M) 2.345ms"
        );
    }

    /// gc-common w3-f: the per-pause `--verbose:gc` line carries this pause's
    /// TTSP and take-over coverage as space-free `key=value` pairs.
    #[test]
    fn w3f_pause_line_carries_ttsp_and_takeover_coverage() {
        let facts = PauseSafepointFacts {
            ttsp_ns: 41_999,
            straggler: Some(3),
            xt_post_quota_ns: 12_500,
            xt_pass_ns: 30_100,
            xt: (2, 1, 0, 17, 3, 5),
        };
        let line = render_pause_line(7, GcDoor::AllocationFailure, facts, 2_345_678);
        assert_eq!(
            line,
            "[GC] pause: gc=7 door=alloc-failure ttsp_us=41 straggler=3 xt_pass_us=30 \
             xt_post_quota_us=12 collect_us=2345 xt_passes=2 xt_frozen=1 xt_unclassified=0 xt_roots=17 \
             hw_windows=3 hw_roots=5"
        );
        for door in [
            GcDoor::AllocationThreshold,
            GcDoor::AllocationFailure,
            GcDoor::SystemGc,
            GcDoor::MetadataThreshold,
            GcDoor::DiagnosticCommand,
            GcDoor::HeapInspection,
            GcDoor::HeapDump,
            GcDoor::G1CompactionPause,
            GcDoor::G1HumongousAllocation,
        ] {
            let l = render_pause_line(0, door, facts, 0);
            assert!(
                l.split(' ').skip(2).all(|kv| kv.split_once('=').is_some()),
                "every field after `[GC] pause:` is key=value: {l}"
            );
        }
        // gc-common w5-a: no participating arrival spells `none`, like the
        // exit line's `max_straggler=`.
        let none = PauseSafepointFacts {
            straggler: None,
            ..facts
        };
        assert!(render_pause_line(1, GcDoor::SystemGc, none, 0).contains(" straggler=none "));
    }

    /// gc-common w5-a (`common-w4f-gc-event-durations-sampled-after-release`):
    /// a sealed collection hands the pause line and the `-Xlog:gc` line the
    /// SAME duration, and its after-heap figures, however late they render.
    #[test]
    fn w5a_a_sealed_collection_reports_one_duration_everywhere() {
        let t0 = std::time::Instant::now();
        let end = t0 + std::time::Duration::from_micros(2_345);
        let start = GcEventStart {
            t0,
            wall_start_ns: 0,
            heap_used_before: 24 * 1024 * 1024,
            heap_committed_before: 96 * 1024 * 1024,
            zgc_moved_before: 0,
            events: true,
            beans: None,
            pause_line: None,
            sealed: Some((end, 3 * 1024 * 1024, 96 * 1024 * 1024)),
            beans_after: None,
            backend_counted: false,
            young_only_cycles_before: 0,
            g1_mixed_before: None,
            g1_old_pauses_before: None,
            cpu_start: None,
            metaspace_before: None,
            classes_unloaded_before: 0,
            tlab_totals: None,
            allocated_at_start: 0,
            heap_debug_before: None,
            heap_debug_after: None,
            g1_heap_debug_before: None,
            g1_heap_debug_after: None,
            escalation: None,
            zgc_pause_split: None,
            g1_regions_before: None,
            g1_regions_after: None,
            g1_prepare_before: None,
            g1_concurrent_start: false,
            g1_marking_idle: false,
            g1_cycle_seq_before: None,
            g1_evacuation_failed: false,
            full_scope_id: None,
            zgc_causes: None,
            zgc_mark_continue: None,
            zgc_conc_mark_ns: 0,
            zgc_cycle_start: None,
        };
        // Rendering happens "later" -- the sealed end does not move.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let (duration_ns, after) = start.end_facts();
        assert_eq!(duration_ns, 2_345_000);
        assert_eq!(after, Some((3 * 1024 * 1024, 96 * 1024 * 1024)));
        let facts = PauseSafepointFacts {
            ttsp_ns: 0,
            straggler: None,
            xt_post_quota_ns: 0,
            xt_pass_ns: 0,
            xt: (0, 0, 0, 0, 0, 0),
        };
        let pause = render_pause_line(4, GcDoor::AllocationThreshold, facts, duration_ns);
        let (heap_after, committed) = after.unwrap();
        let xlog = render_xlog_gc_line(
            4,
            "Pause Young",
            GcDoor::AllocationThreshold.cause(),
            start.heap_used_before,
            heap_after,
            committed,
            duration_ns,
        );
        assert!(pause.contains(" collect_us=2345 "), "{pause}");
        assert_eq!(
            xlog,
            "GC(4) Pause Young (Allocation Failure) 24M->3M(96M) 2.345ms"
        );
        // Unsealed: measured to "now", and no heap figures (the caller reads
        // them live, the pre-w5-a behaviour).
        let unsealed = GcEventStart {
            sealed: None,
            ..start
        };
        let (late_ns, late_after) = unsealed.end_facts();
        assert!(late_ns >= 2_000_000);
        assert_eq!(late_after, None);
        // zr z1/tr: off ZGC (no causes captured) every door keeps its own.
        for door in [
            GcDoor::AllocationThreshold,
            GcDoor::AllocationFailure,
            GcDoor::SystemGc,
            GcDoor::MetadataThreshold,
        ] {
            assert_eq!(unsealed.cause(door), door.cause());
        }
    }

    /// zr z3/t2: the occupancy sites are exactly the doors an occupancy
    /// verdict initiates; the generic failure retries are not among them; the
    /// per-thread classification is scoped to the pause and restored.
    #[test]
    fn zr3t2_forced_sites_classify_exactly_and_the_scope_restores() {
        use super::super::gc_and_alloc::zgc_forced_site_is_occupancy as occ;
        for site in [
            "tlab-alloc-shaped",
            "jit-newarray-zgc-headroom",
            "vm-exec-native-array",
            "vm-exec-pressure",
            "jit-box-pressure",
            "jit-object-young-probe",
            "jit-anewarray-young-probe",
            "tlab-refill-wedge-gate",
        ] {
            assert!(occ(site), "{site}");
        }
        // zt t21/tg10: the pressure door's refused-allocation arm
        // (`CRATONVM_ZGC_PRESSURE_REFUSAL_IS_STALL`) is a failure too.
        for site in [
            "jit-helpers",
            "vm-exec",
            "unlabelled",
            "jit-ldc-string",
            "tlab-refill-wedge",
            "vm-exec-pressure-refused",
        ] {
            assert!(!occ(site), "{site} is a failure retry");
        }
        assert!(!zgc_forced_door_occupancy(), "default: a failed allocation");
        let inner = with_zgc_forced_door_occupancy(true, || {
            let nested = with_zgc_forced_door_occupancy(false, zgc_forced_door_occupancy);
            (nested, zgc_forced_door_occupancy())
        });
        assert_eq!(
            inner,
            (false, true),
            "nested scope restores the outer value"
        );
        assert!(!zgc_forced_door_occupancy(), "restored after the pause");
    }

    /// zt t31/zc11: a closed cycle's cycle record starts at the cycle's start;
    /// its pause records keep their own spans; a later start moves nothing.
    #[test]
    fn zt31zc11_a_closed_cycles_record_starts_at_its_mark_start() {
        use cratonvm_gc::gc_metrics::{GcNotificationRecord, GC_ACTION_PAUSE};
        let t = std::time::Instant::now();
        let at = |us: u64| t + std::time::Duration::from_micros(us);
        let rec = |action: &'static str| GcNotificationRecord {
            collector: "ZGC Major Cycles",
            action,
            cause: "System.gc()",
            id: 1,
            start: at(5_000),
            end: at(6_000),
            timestamp_ms: 0,
            before: Vec::new(),
            after: Vec::new(),
        };
        let mut recs = vec![rec(GC_ACTION_PAUSE), rec("end of GC cycle")];
        cycle_records_start_at(&mut recs, None);
        assert_eq!(recs[1].start, at(5_000));
        cycle_records_start_at(&mut recs, Some(at(9_000)));
        assert_eq!(recs[1].start, at(5_000), "never later than its own");
        cycle_records_start_at(&mut recs, Some(at(1_000)));
        assert_eq!((recs[0].start, recs[1].start), (at(5_000), at(1_000)));
    }

    /// zt t31/zc11 (program page 1b): a requested cycle's mark start carries
    /// its door's cause, only inside the scope, nested scopes restoring.
    #[test]
    fn zt31zc11_a_requested_mark_start_carries_the_door_cause() {
        assert_eq!(zgc_mark_start_door_cause(), None, "default: the rule's cause");
        let seen = with_zgc_mark_start_door(GcDoor::SystemGc, || {
            let nested =
                with_zgc_mark_start_door(GcDoor::JvmtiForceGc, zgc_mark_start_door_cause);
            (nested, zgc_mark_start_door_cause())
        });
        assert_eq!(
            seen,
            (
                Some("JvmtiEnv ForceGarbageCollection"),
                Some("System.gc()")
            )
        );
        assert_eq!(zgc_mark_start_door_cause(), None, "restored after the mark start");
    }

    /// zr z9/hp: a ZGC collection writes no level-1 pause rows unless the
    /// HotSpot phase set is killed; every other collector keeps its rows.
    #[test]
    fn zr9hp_zgc_writes_no_pause_level1_rows_by_default() {
        // ZGC facts present (ZGC with its JFR events on), switch on: none.
        assert!(!zgc_jfr_pause_level1_rows(true, true));
        // Kill control: the z4/jf laps come back.
        assert!(zgc_jfr_pause_level1_rows(true, false));
        // Generational / G1 / ZGC with CRATONVM_ZGC_JFR_EVENTS=0: unchanged.
        assert!(zgc_jfr_pause_level1_rows(false, true));
        assert!(zgc_jfr_pause_level1_rows(false, false));
    }

    /// zs s7/jf: a ZGC collection's pause is written as HotSpot 25's named
    /// pauses, cut at the split's boundaries, contiguous and summing to the
    /// pause; a cycle whose mark start was its own pause has two; no split
    /// (or one that does not fit) leaves the caller's one `GC Pause` row.
    #[test]
    fn zs7jf_zgc_pause_rows_are_hotspots_named_pauses() {
        use cratonvm_gc::gc_metrics::ZgcPauseSplit;
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let at = |us: u64| t0 + Duration::from_micros(us);
        let wall = 1_000_000_000u64;
        // A stop-the-world major collection of 1 ms: 100 us to the end of
        // the snapshot, 700 us to the end of reference processing.
        let split = ZgcPauseSplit::new(Some(at(100)), at(700));
        let rows = zgc_hotspot_pause_rows(Some(&split), t0, wall, 1_000_000, true);
        assert_eq!(
            rows,
            vec![
                ("Pause Mark Start (Major)", wall, 100_000),
                ("Pause Mark End", wall + 100_000, 600_000),
                ("Pause Relocate Start", wall + 700_000, 300_000),
            ]
        );
        assert_eq!(rows.iter().map(|r| r.2).sum::<u64>(), 1_000_000);
        // A young collection's mark start has no `(Major)`.
        let rows = zgc_hotspot_pause_rows(Some(&split), t0, wall, 1_000_000, false);
        assert_eq!(rows[0].0, "Pause Mark Start");
        // A concurrently marked cycle: its mark start was its own pause.
        let conc = ZgcPauseSplit::new(None, at(400));
        let rows = zgc_hotspot_pause_rows(Some(&conc), t0, wall, 1_000_000, true);
        assert_eq!(
            rows,
            vec![
                ("Pause Mark End", wall, 400_000),
                ("Pause Relocate Start", wall + 400_000, 600_000),
            ]
        );
        // No split, or one past the pause's end: the caller's one row.
        assert!(zgc_hotspot_pause_rows(None, t0, wall, 1_000_000, true).is_empty());
        let late = ZgcPauseSplit::new(Some(at(100)), at(2_000));
        assert!(zgc_hotspot_pause_rows(Some(&late), t0, wall, 1_000_000, true).is_empty());
    }

    /// zs s10/jf3 (ported by zt t1/jf): a concurrent cycle's mark-start pause
    /// becomes the first row of its closing collection and the cycle span
    /// starts there; another cycle's pause, a collection with no named rows,
    /// or a pause after the collection's end leaves rows and span as they
    /// were (positive control: the matching case changes both).
    #[test]
    fn zgc_xlog_joins_mark_start_pause_without_jfr() {
        let mut rows = vec![
            ("Pause Mark End", 200, 1_500_000),
            ("Pause Relocate Start", 1_700_200, 800_000),
        ];
        let span = zgc_join_initial_pause(7, Some((7, 50, 2_000_000)), &mut rows, true, 2_500_200);
        assert_eq!(span, Some((50, 2_500_150)));
        assert_eq!(rows[0], ("Pause Mark Start (Major)", 50, 2_000_000));
        let frames = render_xlog_zgc_phase_frames(&ZgcXlogPhaseFacts {
            gc_id: 7,
            minor: false,
            precleans: false,
            pauses: &rows,
            attempts: &[],
            conc_mark_ns: 10_000_000,
            heap_before: 700 * 1024 * 1024,
            heap_after: 600 * 1024 * 1024,
            capacity: 1500 * 1024 * 1024,
            duration_ns: 2_500_000,
        });
        let joined = frames
            .iter()
            .flat_map(|f| f.head.iter().chain(std::iter::once(&f.tail)))
            .find(|line| line.contains("Pause Mark Start (Major)"))
            .expect("Pause Mark Start (Major) line");
        assert!(joined.contains("2.000ms"), "got: {joined}");
    }

    #[test]
    fn zs10jf3_concurrent_cycle_shape_joins_the_mark_start_pause() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let rows = || {
            vec![
                (ZGC_PAUSE_MARK_END, 5_000u64, 300u64),
                (ZGC_PAUSE_RELOCATE_START, 5_300, 200),
            ]
        };
        let mut pauses = rows();
        let span = zgc_cycle_shape(&shared, 4, Some((4, 1_000, 100)), &mut pauses, true, 5_500);
        assert_eq!(span, Some((1_000, 4_500)));
        assert_eq!(pauses[0], ("Pause Mark Start (Major)", 1_000, 100));
        assert_eq!(pauses.len(), 3);
        let mut minor = rows();
        let _ = zgc_cycle_shape(&shared, 4, Some((4, 1_000, 100)), &mut minor, false, 5_500);
        assert_eq!(minor[0].0, "Pause Mark Start");
        // Another cycle's pause: unchanged.
        let mut other = rows();
        assert_eq!(
            zgc_cycle_shape(&shared, 5, Some((4, 1_000, 100)), &mut other, true, 5_500),
            None
        );
        assert_eq!(other, rows());
        // No named rows (the s7 `GC Pause` shape): unchanged.
        let mut none: Vec<(&'static str, u64, u64)> = Vec::new();
        assert_eq!(
            zgc_cycle_shape(&shared, 4, Some((4, 1_000, 100)), &mut none, true, 5_500),
            None
        );
        assert!(none.is_empty());
        // A pause that started after the collection ended: unchanged.
        let mut late = rows();
        assert_eq!(
            zgc_cycle_shape(&shared, 4, Some((4, 9_000, 100)), &mut late, true, 5_500),
            None
        );
        assert_eq!(late, rows());
        assert_eq!(
            zgc_cycle_shape(&shared, 4, None, &mut rows(), true, 5_500),
            None
        );
    }

    /// zt t24/zc8 (`CRATONVM_ZGC_MARK_CONTINUE`): a cycle whose first Pause
    /// Mark End failed prints HotSpot 25's order -- the mark start, the failed
    /// `O: Pause Mark End`, `O: Concurrent Mark Continue`, then the mark end
    /// that succeeded and the relocate start -- one pair per failed attempt;
    /// no attempt, no change; no mark-end line (a minor), appended.
    #[test]
    fn zt24zc8_failed_mark_ends_print_before_the_one_that_succeeded() {
        let rows = [
            (ZGC_PAUSE_MARK_START_MAJOR, 0u64, 11_000u64),
            (ZGC_PAUSE_MARK_END, 0, 16_000),
            (ZGC_PAUSE_RELOCATE_START, 0, 10_400),
        ];
        let mut lines = render_xlog_zgc_pause_lines(3, &rows, false);
        let unchanged = lines.clone();
        splice_xlog_zgc_mark_continue_lines(3, &mut lines, &[]);
        assert_eq!(lines, unchanged, "no attempt: unchanged");
        splice_xlog_zgc_mark_continue_lines(3, &mut lines, &[(21_000, 1_234_000), (9_000, 5_000)]);
        assert_eq!(
            lines,
            vec![
                "GC(3) Y: Pause Mark Start (Major) 0.011ms".to_string(),
                "GC(3) O: Pause Mark End 0.021ms".to_string(),
                "GC(3) O: Concurrent Mark Continue 1.234ms".to_string(),
                "GC(3) O: Pause Mark End 0.009ms".to_string(),
                "GC(3) O: Concurrent Mark Continue 0.005ms".to_string(),
                "GC(3) O: Pause Mark End 0.016ms".to_string(),
                "GC(3) O: Pause Relocate Start 0.010ms".to_string(),
            ]
        );
        let mut minor = render_xlog_zgc_pause_lines(7, &[(ZGC_PAUSE_MARK_START, 0, 12_000)], true);
        splice_xlog_zgc_mark_continue_lines(7, &mut minor, &[(1_000, 2_000)]);
        assert_eq!(minor.len(), 3, "appended after a minor's lines");
        // The pause kind: its own `[GC] pause:` token, a `ZGC Major Pauses`
        // pause, a counted safepoint, `ZMarkEndOld` under `-Xlog:safepoint`.
        let kind = NonCollectionPause::ZgcMarkEndContinue;
        assert_eq!(kind.token(), "zgc-mark-end");
        assert_eq!(
            kind.bean_pause().map(|p| p.0),
            Some(cratonvm_gc::gc_metrics::BackendBeanShape::Zgc)
        );
        assert_eq!(non_collection_safepoint_name(kind), None);
        assert!(!non_collection_pause_is_no_safepoint(kind, None));
        assert_eq!(ZGC_MARK_END_OLD_OPERATION, "ZMarkEndOld");
        // A backdated timer measures from the earlier instant only.
        let early = std::time::Instant::now();
        let mut timer = Some(NonCollectionPauseTimer {
            kind: NonCollectionPause::ZgcMarkEndContinue,
            facts: None,
            t0: std::time::Instant::now(),
            work_ns: None,
            beans: None,
            jfr: None,
            g1_cycle: None,
            folded: None,
            zgc_jfr: None,
        });
        non_collection_pause_backdate(&mut timer, early);
        assert!(timer.as_ref().is_some_and(|t| t.t0 == early));
        non_collection_pause_backdate(&mut timer, std::time::Instant::now());
        assert!(timer.as_ref().is_some_and(|t| t.t0 == early), "never later");
        non_collection_pause_backdate(&mut None, early);
    }

    /// zt t5/jf (R9): HotSpot 25.0.3's ZGC `gc,phases` / `gc,metaspace`
    /// line shapes (local `-Xlog:gc*` capture): `GC(3) Y: Pause Mark Start
    /// (Major) 0.011ms`, `GC(3) O: Pause Mark End 0.016ms`,
    /// `GC(3) O: Metaspace: 3M used, 3M committed, 1088M reserved`; a minor's
    /// lines are `y:`.
    #[test]
    fn zt5jf_zgc_pause_and_metaspace_lines_have_hotspots_shape() {
        let rows = [
            (ZGC_PAUSE_MARK_START_MAJOR, 0u64, 11_000u64),
            (ZGC_PAUSE_MARK_END, 0, 16_000),
            (ZGC_PAUSE_RELOCATE_START, 0, 10_400),
        ];
        assert_eq!(
            render_xlog_zgc_pause_lines(3, &rows, false),
            vec![
                "GC(3) Y: Pause Mark Start (Major) 0.011ms".to_string(),
                "GC(3) O: Pause Mark End 0.016ms".to_string(),
                "GC(3) O: Pause Relocate Start 0.010ms".to_string(),
            ]
        );
        let minor = [(ZGC_PAUSE_MARK_START, 0u64, 12_000u64)];
        assert_eq!(
            render_xlog_zgc_pause_lines(7, &minor, true),
            vec!["GC(7) y: Pause Mark Start 0.012ms".to_string()]
        );
        assert!(render_xlog_zgc_pause_lines(1, &[], false).is_empty());
        let m = 1024 * 1024;
        assert_eq!(
            render_xlog_zgc_metaspace_line(3, false, (3 * m + 5, 3 * m + 9, 1088 * m)),
            "GC(3) O: Metaspace: 3M used, 3M committed, 1088M reserved"
        );
        assert_eq!(
            render_xlog_zgc_metaspace_line(4, true, (0, 0, 0)),
            "GC(4) y: Metaspace: 0M used, 0M committed, 0M reserved"
        );
    }

    /// zt t7/x4b: a generation's `gc,heap` block (capacity triple + `Heap
    /// Statistics:` table) against a HotSpot 25.0.3 capture.
    #[test]
    fn zt7x4b_zgc_heap_block_has_hotspots_layout() {
        // Local HotSpot 25.0.3 capture (`-XX:+UseZGC -Xmx256m
        // -XX:ZCollectionInterval=0.05 -Xlog:gc+heap`), `GC(0) Y:` block,
        // byte for byte (trailing blanks included).
        const HS: [&str; 8] = [
            "GC(0) Y: Min Capacity: 8M(3%)",
            "GC(0) Y: Max Capacity: 256M(100%)",
            "GC(0) Y: Soft Max Capacity: 256M(100%)",
            "GC(0) Y: Heap Statistics:",
            "GC(0) Y:                Mark Start          Mark End        Relocate Start      Relocate End           High               Low         ", // literal-continuation-ok: a source witness or a HotSpot log table, spaces are the text
            "GC(0) Y:  Capacity:      254M (99%)         254M (99%)         254M (99%)         254M (99%)         254M (99%)         254M (99%)    ",
            "GC(0) Y:      Free:      254M (99%)         252M (98%)         252M (98%)         252M (98%)         254M (99%)         252M (98%)    ",
            "GC(0) Y:      Used:        2M (1%)            4M (2%)            4M (2%)            4M (2%)            4M (2%)            2M (1%)     ", // literal-continuation-ok: a source witness or a HotSpot log table, spaces are the text
        ];
        let m = 1024 * 1024;
        let used = [2 * m, 4 * m, 4 * m, 4 * m, 4 * m, 2 * m];
        let caps = [8 * m, 256 * m, 256 * m, 254 * m];
        let block = render_xlog_zgc_heap_block(0, "Y", caps, used);
        assert_eq!(block.len(), 8);
        // zt t8/x5: every line, `Free` included: HotSpot's free space is
        // `max - used` (254M beside 2M used under `-Xmx256m`), and its
        // `High` / `Low` cells are the free space's own extremes. The t7
        // test pinned `capacity - used` (250M) here.
        for (i, want) in HS.iter().enumerate() {
            assert_eq!(block[i], *want, "line {i}");
        }
        // A second 25.0.3 capture (`-Xmx256m -Xlog:gc+heap`, a `System.gc()`
        // after 1 MiB of live arrays), `GC(0) Y:` Free row: used 26, 28, 28,
        // 18 at the phase points, high 32, low 18 -- free 230, 228, 228, 238,
        // then high 238 (from used low) and low 224 (from used high).
        let used = [26 * m, 28 * m, 28 * m, 18 * m, 32 * m, 18 * m];
        let block = render_xlog_zgc_heap_block(0, "Y", caps, used);
        // Compared token by token (the string-literal ratchet counts space
        // runs), with the column layout pinned by the length.
        let tokens: Vec<&str> = block[6].split_whitespace().collect();
        assert_eq!(
            tokens,
            [
                "GC(0)", "Y:", "Free:", "230M", "(90%)", "228M", "(89%)", "228M", "(89%)", "238M",
                "(93%)", "238M", "(93%)", "224M", "(88%)",
            ]
        );
        assert_eq!(block[6].len(), HS[6].len());
        // The generation prefix is the caller's (`O`, `y`).
        assert!(render_xlog_zgc_heap_block(3, "y", caps, used)[3].starts_with("GC(3) y: Heap"));
    }

    /// zt t9/x6: HotSpot 25.0.3's `Young Generation Statistics:` and `Old
    /// Generation Statistics:` tables, byte for byte against a local capture
    /// (`java -XX:+UseZGC -Xmx256m -Xlog:gc*` over a `System.gc()`, `GC(1)`,
    /// the `Promote All` young block and the old block). The expected lines
    /// spell each space as `.` (the string-literal ratchet counts space
    /// runs); the figures are the capture's, a byte count inside each MiB so
    /// the rounded percentage matches.
    #[test]
    fn zt9x6_generation_statistics_tables_have_hotspots_layout() {
        const YOUNG: [&str; 9] = [
            "GC(1).Y:.Young.Generation.Statistics:",
            "GC(1).Y:................Mark.Start..........Mark.End........Relocate.Start......Relocate.End....",
            "GC(1).Y:......Used:.......36M.(14%)..........36M.(14%)..........20M.(8%)............0M.(0%).....",
            "GC(1).Y:......Live:.........-.................4M.(2%)............2M.(1%)............0M.(0%).....",
            "GC(1).Y:...Garbage:.........-................31M.(12%)..........17M.(7%)............0M.(0%).....",
            "GC(1).Y:.Allocated:.........-.................0M.(0%)............0M.(0%)............0M.(0%).....",
            "GC(1).Y:.Reclaimed:.........-..................-................14M.(6%)...........31M.(12%)....",
            "GC(1).Y:..Promoted:.........-..................-.................1M.(0%)............4M.(2%).....",
            "GC(1).Y:.Compacted:.........-..................-..................-.................0M.(0%).....",
        ];
        const OLD: [&str; 8] = [
            "GC(1).O:.Old.Generation.Statistics:",
            "GC(1).O:................Mark.Start..........Mark.End........Relocate.Start......Relocate.End....",
            "GC(1).O:......Used:........6M.(2%)............6M.(2%)............6M.(2%)............6M.(2%).....",
            "GC(1).O:......Live:.........-.................4M.(2%)............4M.(2%)............4M.(2%).....",
            "GC(1).O:...Garbage:.........-.................1M.(1%)............1M.(1%)............1M.(1%).....",
            "GC(1).O:.Allocated:.........-.................0M.(0%)............0M.(0%)............0M.(0%).....",
            "GC(1).O:.Reclaimed:.........-..................-.................0M.(0%)............0M.(0%).....",
            "GC(1).O:.Compacted:.........-..................-..................-.................0M.(0%).....",
        ];
        let m = 1024 * 1024;
        let young = ZgcXlogGenerationFigures {
            used: [36 * m, 36 * m, 20 * m, 0],
            live: [4 * m, 2 * m, 0],
            garbage: [31 * m, 17 * m, 0],
            allocated: [0; 3],
            reclaimed: [14 * m + 900 * 1024, 31 * m],
            promoted: [m, 4 * m],
            compacted: 0,
        };
        let got = render_xlog_zgc_generation_statistics(1, "Y", true, 256 * m, &young);
        assert_eq!(got.len(), YOUNG.len());
        for (i, want) in YOUNG.iter().enumerate() {
            assert_eq!(got[i].replace(' ', "."), *want, "young line {i}");
        }
        let half = m / 2;
        let old = ZgcXlogGenerationFigures {
            used: [6 * m; 4],
            live: [4 * m; 3],
            garbage: [m + half; 3],
            ..ZgcXlogGenerationFigures::default()
        };
        let got = render_xlog_zgc_generation_statistics(1, "O", false, 256 * m, &old);
        assert_eq!(got.len(), OLD.len(), "the old table has no Promoted row");
        for (i, want) in OLD.iter().enumerate() {
            assert_eq!(got[i].replace(' ', "."), *want, "old line {i}");
        }
    }

    /// zt t9/x6: the per-collection `gc,heap` line count is HotSpot 25.0.3's
    /// (local capture: a `Warmup` major 33 lines, a `System.gc()` major 50,
    /// i.e. two young blocks of 17 and an old one of 16); a minor is one young
    /// block (17). Kill arm (`gen_tables == false`): t7's two 8-line blocks.
    /// The figures of a major: the first young block promotes every survivor
    /// and the old block reads the occupancy after it.
    #[test]
    fn zt9x6_heap_blocks_follow_hotspots_generation_split() {
        let m = 1024 * 1024;
        let caps = [8 * m, 256 * m, 256 * m, 254 * m];
        let count = |blocks: &[(&'static str, Vec<String>)]| -> Vec<(&'static str, usize)> {
            blocks.iter().map(|(g, b)| (*g, b.len())).collect()
        };
        let system_gc = zgc_xlog_heap_blocks(
            1,
            false,
            "System.gc()",
            caps,
            36 * m,
            6 * m,
            true,
            None,
            false,
        );
        assert_eq!(count(&system_gc), [("Y", 17), ("Y", 17), ("O", 16)]);
        assert_eq!(system_gc.iter().map(|(_, b)| b.len()).sum::<usize>(), 50);
        let warmup =
            zgc_xlog_heap_blocks(0, false, "Warmup", caps, 26 * m, 18 * m, true, None, false);
        assert_eq!(count(&warmup), [("Y", 17), ("O", 16)]);
        // zt t16/gn6 (item 12): the same `Warmup` major begun with an
        // allocation failure latched pre-cleans, as HotSpot's
        // `is_alloc_stalling_for_old` clause does; a minor never does.
        let stalled =
            zgc_xlog_heap_blocks(0, false, "Warmup", caps, 26 * m, 18 * m, true, None, true);
        assert_eq!(count(&stalled), [("Y", 17), ("Y", 17), ("O", 16)]);
        let minor = zgc_xlog_heap_blocks(
            2,
            true,
            "Allocation Rate",
            caps,
            30 * m,
            10 * m,
            true,
            None,
            false,
        );
        assert_eq!(count(&minor), [("y", 17)]);
        assert!(minor[0].1[8].starts_with("GC(2) y: Young Generation Statistics:"));
        let killed = zgc_xlog_heap_blocks(
            1,
            false,
            "System.gc()",
            caps,
            36 * m,
            6 * m,
            false,
            None,
            false,
        );
        assert_eq!(count(&killed), [("Y", 8), ("O", 8)]);
        let words = |line: &str| line.split_whitespace().collect::<Vec<_>>().join(" ");
        // The first young block: Used 36M -> 0M, Promoted 6M at relocate end.
        assert_eq!(
            words(&system_gc[0].1[10]),
            "GC(1) Y: Used: 36M (14%) 36M (14%) 36M (14%) 0M (0%)"
        );
        assert_eq!(words(&system_gc[0].1[15]), "GC(1) Y: Promoted: - - 0M (0%) 6M (2%)");
        // The Collect Roots block: the young generation is empty.
        let roots = words(&system_gc[1].1[10]);
        assert_eq!(roots, "GC(1) Y: Used: 0M (0%) 0M (0%) 0M (0%) 0M (0%)");
        // The old block's heap table reads the occupancy after the young half.
        assert_eq!(
            words(&system_gc[2].1[7]),
            "GC(1) O: Used: 6M (2%) 6M (2%) 6M (2%) 6M (2%) 6M (2%) 6M (2%)"
        );
        // Every pre-cleaning cause of `should_preclean_young`, and not the
        // others.
        const PRECLEAN: [&str; 4] = [
            "System.gc()",
            "Diagnostic Command",
            "Allocation Stall",
            "Heap Dump Initiated GC",
        ];
        for cause in PRECLEAN {
            assert!(zgc_major_precleans_young(cause), "{cause}");
        }
        const PARTIAL: [&str; 5] = [
            "Warmup",
            "Timer",
            "Proactive",
            "Allocation Rate",
            "Metadata GC Threshold",
        ];
        for cause in PARTIAL {
            assert!(!zgc_major_precleans_young(cause), "{cause}");
        }
    }

    /// zt t31/x10 (page `zgc-zt13x10-review-residuals-*` item 2,
    /// `CRATONVM_ZGC_XLOG_LOAD_MMU`): HotSpot's `Load:` / `MMU:` texts, the
    /// MMU over a 1 ms pause (2 ms window: half paused) and its running
    /// minimum, and the lines' place before each generation's `Metaspace:`.
    #[test]
    fn zt31x10_load_and_mmu_lines_follow_hotspots_zstat() {
        assert_eq!(
            render_xlog_zgc_load([51.79, 45.06, 28.38], 16),
            "Load: 51.79 (324%) / 45.06 (282%) / 28.38 (177%)"
        );
        let mut mmu = crate::vm::realms::heap_realm::ZStatMmu::default();
        let t = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let first = mmu.register_pause(t, t + std::time::Duration::from_millis(1));
        assert_eq!(
            render_xlog_zgc_mmu(first),
            "MMU: 2ms/50.0%, 5ms/80.0%, 10ms/90.0%, 20ms/95.0%, 50ms/98.0%, 100ms/99.0%"
        );
        // A later, shorter pause cannot raise a minimum.
        let later = t + std::time::Duration::from_secs(1);
        let second = mmu.register_pause(later, later + std::time::Duration::from_micros(100));
        assert_eq!(second, first, "running minima");
        let stats = ZgcXlogStatLines {
            load: Some("Load: 1.00 (6%) / 1.00 (6%) / 1.00 (6%)".to_string()),
            mmu: Some(render_xlog_zgc_mmu(first)),
        };
        let pauses = vec!["GC(1) Y: Pause Mark Start (Major) 0.1ms".to_string()];
        let lines = zgc_xlog_collection_lines_with(
            1,
            false,
            true,
            pauses,
            Some((1 << 20, 1 << 20, 1 << 30)),
            Vec::new(),
            &stats,
        );
        let tags: Vec<String> = lines.iter().map(|(t, _)| t.to_string()).collect();
        assert_eq!(
            tags,
            [
                "gc,phases",
                "gc,load",
                "gc,mmu",
                "gc,metaspace",
                "gc,load",
                "gc,mmu",
                "gc,metaspace"
            ]
        );
        assert!(lines[1].1.starts_with("GC(1) Y: Load: "));
        assert!(lines[5].1.starts_with("GC(1) O: MMU: 2ms/"));
        // Off (no stat lines): the t9 builder's lines.
        assert_eq!(
            zgc_xlog_collection_lines_with(
                1,
                true,
                true,
                Vec::new(),
                None,
                Vec::new(),
                &ZgcXlogStatLines::default()
            ),
            zgc_xlog_collection_lines(1, true, true, Vec::new(), None, Vec::new())
        );
    }

    /// zt t31/x10 (page `zgc-zt13x10-review-residuals-*` item 1,
    /// `CRATONVM_ZGC_XLOG_EDGE_BOOK`): the booked edges reach the tables. A
    /// major that closed a concurrent cycle whose mark started at 20M prints
    /// Mark Start at 20M, the 16M the mutators allocated since as the young
    /// table's `Allocated`, and the slide's 3M as the old table's
    /// `Compacted`; a minor carries its own `Compacted`. Control: default
    /// edges (a stop-the-world mark, no slide) print the tables as before.
    #[test]
    fn zt31x10_the_edge_book_reaches_the_generation_tables() {
        let m = 1024 * 1024;
        let caps = [8 * m, 256 * m, 256 * m, 254 * m];
        let words = |line: &str| line.split_whitespace().collect::<Vec<_>>().join(" ");
        let edges = ZgcXlogEdges {
            mark_start_used: Some(20 * m),
            compacted: 3 * m,
        };
        let major = zgc_xlog_heap_blocks_with(
            1, false, "Warmup", caps, 36 * m, 6 * m, true, None, false, edges,
        );
        let (y, o) = (&major[0].1, &major[1].1);
        assert_eq!(
            words(&y[7]),
            "GC(1) Y: Used: 20M (8%) 36M (14%) 36M (14%) 6M (2%) 36M (14%) 6M (2%)"
        );
        assert_eq!(words(&y[10]), "GC(1) Y: Used: 20M (8%) 36M (14%) 36M (14%) 0M (0%)");
        assert_eq!(words(&y[13]), "GC(1) Y: Allocated: - 16M (6%) 16M (6%) 16M (6%)");
        assert_eq!(words(&o[13]), "GC(1) O: Allocated: - 0M (0%) 0M (0%) 0M (0%)");
        assert_eq!(words(&o[15]), "GC(1) O: Compacted: - - - 3M (1%)");
        let minor = zgc_xlog_heap_blocks_with(
            2,
            true,
            "Allocation Rate",
            caps,
            30 * m,
            10 * m,
            true,
            None,
            false,
            ZgcXlogEdges {
                mark_start_used: None,
                compacted: 2 * m,
            },
        );
        assert_eq!(words(&minor[0].1[16]), "GC(2) y: Compacted: - - - 2M (1%)");
        assert_eq!(words(&minor[0].1[13]), "GC(2) y: Allocated: - 0M (0%) 0M (0%) 0M (0%)");
        for (minor_cycle, cause) in [(false, "System.gc()"), (true, "Allocation Rate")] {
            assert_eq!(
                zgc_xlog_heap_blocks_with(
                    1,
                    minor_cycle,
                    cause,
                    caps,
                    36 * m,
                    6 * m,
                    true,
                    None,
                    false,
                    ZgcXlogEdges::default(),
                ),
                zgc_xlog_heap_blocks(1, minor_cycle, cause, caps, 36 * m, 6 * m, true, None, false),
                "default edges: the tables as before ({cause})"
            );
        }
    }

    /// zt t10/gn2 (`CRATONVM_ZGC_XLOG_GEN_YOUNG_FIGURES`, `zgc-zt10x7-*` item
    /// 8): with the young split a minor's `y:` table counts the young
    /// generation only -- 30M before / 10M after, an old generation of 8M after
    /// the minor of which 2M it promoted: young 24M at mark start, 2M kept
    /// young, 4M live, 20M garbage reclaimed, 2M promoted. The `Heap
    /// Statistics` table above it and the line count are unchanged. Kill
    /// control (`None`): t9/x6's whole-heap figures.
    #[test]
    fn zt10gn2_a_minors_young_table_counts_the_young_generation() {
        let m = 1024 * 1024;
        let caps = [8 * m, 256 * m, 256 * m, 254 * m];
        let words = |line: &str| line.split_whitespace().collect::<Vec<_>>().join(" ");
        let blocks = |ys: Option<(usize, usize)>| {
            zgc_xlog_heap_blocks(2, true, "Allocation Rate", caps, 30 * m, 10 * m, true, ys, false)
        };
        let young = blocks(Some((8 * m, 2 * m)));
        let whole = blocks(None);
        assert_eq!(young.len(), 1);
        assert_eq!(young[0].1.len(), 17);
        assert_eq!(young[0].1[..8], whole[0].1[..8], "the heap table is whole-heap");
        let table: Vec<String> = young[0].1[10..16].iter().map(|l| words(l)).collect();
        assert_eq!(
            table,
            [
                "GC(2) y: Used: 24M (9%) 24M (9%) 24M (9%) 2M (1%)",
                "GC(2) y: Live: - 4M (2%) 4M (2%) 2M (1%)",
                "GC(2) y: Garbage: - 20M (8%) 20M (8%) 0M (0%)",
                "GC(2) y: Allocated: - 0M (0%) 0M (0%) 0M (0%)",
                "GC(2) y: Reclaimed: - - 0M (0%) 20M (8%)",
                "GC(2) y: Promoted: - - 0M (0%) 2M (1%)",
            ]
        );
        assert_eq!(words(&whole[0].1[10]), "GC(2) y: Used: 30M (12%) 30M (12%) 30M (12%) 10M (4%)");
        assert_eq!(words(&whole[0].1[15]), "GC(2) y: Promoted: - - 0M (0%) 0M (0%)");
        // An old generation larger than the heap before (a stale split) can
        // not underflow: every young figure saturates at 0.
        let stale = blocks(Some((40 * m, 0)));
        assert_eq!(words(&stale[0].1[10]), "GC(2) y: Used: 0M (0%) 0M (0%) 0M (0%) 0M (0%)");
    }

    /// zt t9/x6 (`CRATONVM_ZGC_XLOG_GENERATION_ORDER`): a major's lines in
    /// HotSpot 25.0.3's `-Xlog:gc*` order -- `Y:` pauses, a `Y: Metaspace:`
    /// line before each young block, then the `O:` pauses, metaspace line and
    /// block. Kill arm: the t7 order (every pause, one `O:` metaspace line,
    /// every block). A minor is unchanged.
    #[test]
    fn zt9x6_a_major_prints_generation_by_generation() {
        use crate::runtime::unified_logging::LogTag;
        let m = 1024 * 1024;
        let pauses = || {
            vec![
                "GC(1) Y: Pause Mark Start (Major) 0.011ms".to_string(),
                "GC(1) O: Pause Mark End 0.016ms".to_string(),
                "GC(1) O: Pause Relocate Start 0.010ms".to_string(),
            ]
        };
        let blocks = || {
            vec![
                ("Y", vec!["GC(1) Y: a".to_string()]),
                ("Y", vec!["GC(1) Y: b".to_string()]),
                ("O", vec!["GC(1) O: c".to_string()]),
            ]
        };
        let meta = Some((9 * m as u64, 9 * m as u64, 1088 * m as u64));
        let grouped = zgc_xlog_collection_lines(1, false, true, pauses(), meta, blocks());
        let ms = |g: &str| format!("GC(1) {g}: Metaspace: 9M used, 9M committed, 1088M reserved");
        let want = vec![
            (LogTag::GcPhases, "GC(1) Y: Pause Mark Start (Major) 0.011ms".to_string()),
            (LogTag::GcMetaspace, ms("Y")),
            (LogTag::GcHeap, "GC(1) Y: a".to_string()),
            (LogTag::GcMetaspace, ms("Y")),
            (LogTag::GcHeap, "GC(1) Y: b".to_string()),
            (LogTag::GcPhases, "GC(1) O: Pause Mark End 0.016ms".to_string()),
            (LogTag::GcPhases, "GC(1) O: Pause Relocate Start 0.010ms".to_string()),
            (LogTag::GcMetaspace, ms("O")),
            (LogTag::GcHeap, "GC(1) O: c".to_string()),
        ];
        assert_eq!(grouped, want);
        // Kill arm: the t7 order.
        let flat = zgc_xlog_collection_lines(1, false, false, pauses(), meta, blocks());
        let flat_text: Vec<&str> = flat.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(flat_text[3], ms("O"));
        assert_eq!(&flat_text[4..], ["GC(1) Y: a", "GC(1) Y: b", "GC(1) O: c"]);
        // No `gc,heap`: one young metaspace line still precedes the old pauses.
        let no_heap = zgc_xlog_collection_lines(1, false, true, pauses(), meta, Vec::new());
        assert_eq!(no_heap.len(), 5);
        assert_eq!(no_heap[1].1, ms("Y"));
        assert_eq!(no_heap[4].1, ms("O"));
        // Nothing selected: nothing printed.
        assert!(zgc_xlog_collection_lines(1, false, true, Vec::new(), None, Vec::new()).is_empty());
        // A minor ignores the grouping.
        let minor = zgc_xlog_collection_lines(
            4,
            true,
            true,
            vec!["GC(4) y: Pause Mark Start 0.012ms".to_string()],
            meta,
            vec![("y", vec!["GC(4) y: d".to_string()])],
        );
        let minor_text: Vec<&str> = minor.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(minor_text[1], "GC(4) y: Metaspace: 9M used, 9M committed, 1088M reserved");
        assert_eq!(minor_text.len(), 3);
    }

    /// zt t31/zc11 (program page 1f residual R6,
    /// `CRATONVM_ZGC_XLOG_PHASE_FRAMES`): a major's `gc,phases` block in
    /// HotSpot 25.0.4's shape (host capture `-Xlog:gc,gc+phases`, `UseZGC`):
    /// a framed young collection with `Pause Mark Start (Major)`, then the
    /// framed old generation with every concurrent phase line, the failed
    /// attempts before the closing `Pause Mark End`; a `System.gc()` major
    /// pre-cleans with a `(Promote All)` collection first; a minor is one `y:`
    /// frame. Each generation's metaspace line and heap block sit before its
    /// frame end.
    #[test]
    fn zt31zc11_the_phase_block_is_hotspots_framed_cycle() {
        use crate::runtime::unified_logging::LogTag;
        let m = 1024 * 1024;
        let pauses = [
            (ZGC_PAUSE_MARK_START_MAJOR, 0, 11_000),
            (ZGC_PAUSE_MARK_END, 0, 16_000),
            (ZGC_PAUSE_RELOCATE_START, 0, 10_000),
        ];
        let facts = |precleans: bool, minor: bool| ZgcXlogPhaseFacts {
            gc_id: 1,
            minor,
            precleans,
            pauses: &pauses,
            attempts: &[(2_000_000, 3_000_000)],
            conc_mark_ns: 9_000_000,
            heap_before: 12 * m,
            heap_after: 2 * m,
            capacity: 256 * m,
            duration_ns: 25_000_000,
        };
        let text = |frames: Vec<ZgcXlogFrame>| -> Vec<String> {
            frames
                .into_iter()
                .flat_map(|f| f.head.into_iter().chain(std::iter::once(f.tail)))
                .collect()
        };
        let major = text(render_xlog_zgc_phase_frames(&facts(false, false)));
        assert_eq!(
            major,
            [
                "GC(1) Y: Young Generation",
                "GC(1) Y: Pause Mark Start (Major) 0.011ms",
                "GC(1) Y: Concurrent Mark 0.000ms",
                "GC(1) Y: Pause Mark End 0.000ms",
                "GC(1) Y: Concurrent Mark Free 0.000ms",
                "GC(1) Y: Concurrent Reset Relocation Set 0.000ms",
                "GC(1) Y: Concurrent Select Relocation Set 0.000ms",
                "GC(1) Y: Pause Relocate Start 0.000ms",
                "GC(1) Y: Concurrent Relocate 0.000ms",
                "GC(1) Y: Young Generation 12M(5%)->2M(1%) 0.000s",
                "GC(1) O: Old Generation",
                "GC(1) O: Concurrent Mark 4.000ms",
                "GC(1) O: Pause Mark End 2.000ms",
                "GC(1) O: Concurrent Mark Continue 3.000ms",
                "GC(1) O: Pause Mark End 0.016ms",
                "GC(1) O: Concurrent Mark Free 0.000ms",
                "GC(1) O: Concurrent Process Non-Strong 0.000ms",
                "GC(1) O: Concurrent Reset Relocation Set 0.000ms",
                "GC(1) O: Concurrent Select Relocation Set 0.000ms",
                "GC(1) O: Concurrent Remap Roots 0.000ms",
                "GC(1) O: Pause Relocate Start 0.010ms",
                "GC(1) O: Concurrent Relocate 0.000ms",
                "GC(1) O: Old Generation 2M(1%)->2M(1%) 0.025s",
            ]
        );
        let pre = text(render_xlog_zgc_phase_frames(&facts(true, false)));
        assert_eq!(pre[0], "GC(1) Y: Young Generation (Promote All)");
        assert_eq!(pre[1], "GC(1) Y: Pause Mark Start 0.000ms");
        assert_eq!(pre[9], "GC(1) Y: Young Generation (Promote All) 12M(5%)->2M(1%) 0.000s");
        assert_eq!(pre[10], "GC(1) Y: Young Generation (Collect Roots)");
        assert_eq!(pre[11], "GC(1) Y: Pause Mark Start (Major) 0.011ms");
        assert_eq!(pre[19], "GC(1) Y: Young Generation (Collect Roots) 2M(1%)->2M(1%) 0.000s");
        assert_eq!(pre[20], "GC(1) O: Old Generation");
        assert_eq!(pre.len(), major.len() + 10);
        let minor_pauses = [(ZGC_PAUSE_MARK_START, 0, 12_000)];
        let minor = ZgcXlogPhaseFacts {
            pauses: &minor_pauses,
            attempts: &[],
            conc_mark_ns: 0,
            ..facts(false, true)
        };
        let minor = text(render_xlog_zgc_phase_frames(&minor));
        assert_eq!(minor.len(), 10);
        assert_eq!(minor[0], "GC(1) y: Young Generation");
        assert_eq!(minor[1], "GC(1) y: Pause Mark Start 0.012ms");
        assert_eq!(minor[9], "GC(1) y: Young Generation 12M(5%)->2M(1%) 0.025s");
        // No pause rows: no frames (the caller prints the plain lines).
        let none = ZgcXlogPhaseFacts {
            pauses: &[],
            ..facts(false, false)
        };
        assert!(render_xlog_zgc_phase_frames(&none).is_empty());

        // The order: each generation's metaspace line and heap block before
        // its frame end.
        let meta = Some((9 * m as u64, 9 * m as u64, 1088 * m as u64));
        let blocks = vec![
            ("Y", vec!["GC(1) Y: a".to_string()]),
            ("Y", vec!["GC(1) Y: b".to_string()]),
            ("O", vec!["GC(1) O: c".to_string()]),
        ];
        let lines = zgc_xlog_collection_lines_framed(
            1,
            false,
            true,
            Vec::new(),
            render_xlog_zgc_phase_frames(&facts(true, false)),
            meta,
            blocks,
            &ZgcXlogStatLines::default(),
        );
        let at = |want: &str| {
            lines
                .iter()
                .position(|(_, l)| l == want)
                .unwrap_or_else(|| panic!("{want} printed"))
        };
        let ms = |g: &str| format!("GC(1) {g}: Metaspace: 9M used, 9M committed, 1088M reserved");
        assert_eq!(lines[9], (LogTag::GcMetaspace, ms("Y")));
        assert_eq!(lines[10], (LogTag::GcHeap, "GC(1) Y: a".to_string()));
        assert_eq!(lines[11].0, LogTag::GcPhases);
        assert!(lines[11].1.starts_with("GC(1) Y: Young Generation (Promote All) "));
        assert!(at("GC(1) Y: b") < at("GC(1) Y: Young Generation (Collect Roots) 2M(1%)->2M(1%) 0.000s"));
        assert!(at("GC(1) O: c") < at("GC(1) O: Old Generation 2M(1%)->2M(1%) 0.025s"));
        assert!(at("GC(1) Y: b") < at("GC(1) O: Old Generation"));
        assert_eq!(lines.len(), 23 + 10 + 3 + 3);
    }


    /// zt t6/ob4 (R9 tail): HotSpot 25.0.3's ZGC `gc,heap` capacity lines
    /// (local `-Xlog:gc*` capture, `-Xmx256m`): `Min Capacity: 8M(3%)`,
    /// `Max Capacity: 256M(100%)`, `Soft Max Capacity: 256M(100%)`; a soft
    /// max above the max prints the max.
    #[test]
    fn zt6ob4_zgc_capacity_lines_have_hotspots_shape() {
        let m = 1024 * 1024;
        assert_eq!(
            render_xlog_zgc_capacity_lines(0, false, 8 * m, 256 * m, 256 * m),
            [
                "GC(0) O: Min Capacity: 8M(3%)".to_string(),
                "GC(0) O: Max Capacity: 256M(100%)".to_string(),
                "GC(0) O: Soft Max Capacity: 256M(100%)".to_string(),
            ]
        );
        assert_eq!(
            render_xlog_zgc_capacity_lines(5, true, 64 * m, 256 * m, 128 * m),
            [
                "GC(5) y: Min Capacity: 64M(25%)".to_string(),
                "GC(5) y: Max Capacity: 256M(100%)".to_string(),
                "GC(5) y: Soft Max Capacity: 128M(50%)".to_string(),
            ]
        );
        assert_eq!(
            render_xlog_zgc_capacity_lines(1, false, 8 * m, 64 * m, usize::MAX)[2],
            "GC(1) O: Soft Max Capacity: 64M(100%)"
        );
    }

    /// zt t1/jf: the young collection inside a ZGC major promotes every
    /// survivor in the single-generation mode and on the `System.gc()` door;
    /// the generational mode keeps its promotion age otherwise.
    #[test]
    fn zt1jf_major_young_threshold_is_promote_all_unless_generational() {
        assert_eq!(zgc_major_young_threshold(None, "System.gc()"), 0);
        assert_eq!(zgc_major_young_threshold(None, "Allocation Rate"), 0);
        assert_eq!(zgc_major_young_threshold(Some(3), "Warmup"), 3);
        assert_eq!(zgc_major_young_threshold(Some(3), "System.gc()"), 0);
        assert_eq!(zgc_major_young_threshold(Some(u32::MAX), "Proactive"), i32::MAX);
    }

    /// zt t1/jf: the four JFR switches of this lane are read per call (the
    /// file's rule since gci i20/ob), default ON, `=0` kills each.


    /// zt t29/g1 (G1 carry page item 1): `jdk.MetaspaceChunkFreeListSummary`
    /// is written on G1 and Generational too (default ON); the neutral key
    /// kills every collector, the ZGC key only ZGC, and ZGC still needs its
    /// JFR facts.
/// gci i20/ob (`gci-i12sx-vm-statics-ratchet-red-on-dev-by-27-FIXED-20261016`):
    /// the ZGC observability switches of this file are read per call, not
    /// latched by the first reader in the process: a per-thread override
    /// reaches each of them, in both directions, in one test.


    /// zr z2/t2: HotSpot ZGC's stall line, three decimals of milliseconds.
    #[test]
    fn zr2t2_allocation_stall_line_has_hotspots_shape() {
        assert_eq!(
            zgc_allocation_stall_line("main", 204_400),
            "Allocation Stall (main) 0.204ms"
        );
        assert_eq!(
            zgc_allocation_stall_line("worker-1", 12_345_678_901),
            "Allocation Stall (worker-1) 12345.679ms"
        );
    }

    /// zr z1/tr: an allocation-driven ZGC collection is reported under HotSpot
    /// 25 ZGC's cause; explicit doors keep theirs.
    #[test]
    fn zr1_zgc_allocation_doors_take_hotspots_zgc_causes() {
        let t0 = std::time::Instant::now();
        let start = GcEventStart {
            t0,
            wall_start_ns: 0,
            heap_used_before: 0,
            heap_committed_before: 0,
            zgc_moved_before: 0,
            events: false,
            beans: None,
            pause_line: None,
            sealed: None,
            beans_after: None,
            backend_counted: false,
            young_only_cycles_before: 0,
            g1_mixed_before: None,
            g1_old_pauses_before: None,
            cpu_start: None,
            metaspace_before: None,
            classes_unloaded_before: 0,
            tlab_totals: None,
            allocated_at_start: 0,
            heap_debug_before: None,
            heap_debug_after: None,
            g1_heap_debug_before: None,
            g1_heap_debug_after: None,
            escalation: None,
            zgc_pause_split: None,
            g1_regions_before: None,
            g1_regions_after: None,
            g1_prepare_before: None,
            g1_concurrent_start: false,
            g1_marking_idle: false,
            g1_cycle_seq_before: None,
            g1_evacuation_failed: false,
            full_scope_id: None,
            zgc_causes: Some(("Warmup", "Allocation Stall")),
            zgc_mark_continue: None,
            zgc_conc_mark_ns: 0,
            zgc_cycle_start: None,
        };
        assert_eq!(start.cause(GcDoor::AllocationThreshold), "Warmup");
        assert_eq!(start.cause(GcDoor::AllocationFailure), "Allocation Stall");
        assert_eq!(start.cause(GcDoor::SystemGc), "System.gc()");
        assert_eq!(
            start.cause(GcDoor::MetadataThreshold),
            "Metadata GC Threshold"
        );
        // zs s8/jf2 (on its own door, `GcDoor::JvmtiForceGc`, since the
        // 2026-10-18 dev merge): JVMTI ForceGarbageCollection reports
        // HotSpot's JVMTI cause on ZGC too; the `System.gc()` door keeps its own.
        assert_eq!(
            start.cause(GcDoor::JvmtiForceGc),
            "JvmtiEnv ForceGarbageCollection"
        );
        assert_eq!(start.cause(GcDoor::SystemGc), "System.gc()");
        let [first, end] = render_xlog_zgc_lines(
            2,
            ZGC_MAJOR_COLLECTION,
            start.cause(GcDoor::AllocationThreshold),
            0,
            0,
            0,
            0,
        );
        assert_eq!(first, "GC(2) Major Collection (Warmup)");
        assert!(end.starts_with("GC(2) Major Collection (Warmup) "), "{end}");
    }

    /// gc-common w5-a (`common-w4a-non-collection-pauses-have-no-pause-line`):
    /// a non-collection pause prints the collection line's grammar with
    /// `gc=-`, its own door token and `work_us=`.
    #[test]
    fn w5a_non_collection_pause_line_shape() {
        let facts = PauseSafepointFacts {
            ttsp_ns: 7_000,
            straggler: Some(9),
            xt_post_quota_ns: 1_000,
            xt_pass_ns: 2_000,
            xt: (1, 0, 0, 0, 0, 0),
        };
        let line = render_safepoint_pause_line(
            "-",
            NonCollectionPause::G1Remark.token(),
            facts,
            "work_us",
            40_000_000,
        );
        assert_eq!(
            line,
            "[GC] pause: gc=- door=g1-remark ttsp_us=7 straggler=9 xt_pass_us=2 \
             xt_post_quota_us=1 work_us=40000 xt_passes=1 xt_frozen=0 xt_unclassified=0 xt_roots=0 \
             hw_windows=0 hw_roots=0"
        );
        let kinds = [
            NonCollectionPause::FrameTrace,
            NonCollectionPause::HeapDump,
            NonCollectionPause::HeapInspection,
            NonCollectionPause::LoopExit,
            NonCollectionPause::Redefinition,
            NonCollectionPause::GenInitialMark,
            NonCollectionPause::GenRemark,
            NonCollectionPause::ZgcMarkStart,
            NonCollectionPause::ZgcMarkEndContinue,
            NonCollectionPause::G1InitialMark,
            NonCollectionPause::G1Remark,
            NonCollectionPause::IcGrace,
        ];
        let mut tokens: Vec<&str> = kinds.iter().map(|k| k.token()).collect();
        for t in &tokens {
            assert!(!t.contains(' ') && !t.contains('='), "scrapable token: {t}");
            for door in [
                "alloc-threshold",
                "alloc-failure",
                "system-gc",
                "heap-inspection-gc",
                "heap-dump-gc",
            ] {
                assert_ne!(*t, door, "a non-collection token must not alias a GC door");
            }
        }
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), kinds.len(), "tokens are distinct");
        // A sealed timer reports its sealed work time, not the time to print.
        let mut timer = Some(NonCollectionPauseTimer {
            kind: NonCollectionPause::FrameTrace,
            facts: Some(facts),
            t0: std::time::Instant::now(),
            work_ns: None,
            beans: None,
            jfr: None,
            g1_cycle: None,
            folded: None,
            zgc_jfr: None,
        });
        // zr z4/jf: a no-op on a timer with no ZGC JFR row.
        non_collection_pause_note_opened(&mut timer, true);
        non_collection_pause_seal(&mut timer);
        assert!(timer.as_ref().is_some_and(|t| t.work_ns.is_some()));
        non_collection_pause_seal(&mut None);
    }

    /// gc-common w7-f: exactly the marking pauses of the backend a bean set
    /// describes count toward it; frame-trace, heap-dump and Generational's
    /// marking pauses count toward none. G1's carry `No GC` (a concurrent
    /// phase to Micrometer), ZGC's mark start an allocation cause (a pause).
    #[test]
    fn w7f_only_backend_marking_pauses_count_toward_a_bean() {
        use cratonvm_gc::gc_metrics::BackendBeanShape;
        assert_eq!(
            NonCollectionPause::G1Remark.bean_pause(),
            Some((BackendBeanShape::G1, "No GC"))
        );
        assert_eq!(
            NonCollectionPause::G1InitialMark.bean_pause().map(|p| p.0),
            Some(BackendBeanShape::G1)
        );
        assert_eq!(
            NonCollectionPause::ZgcMarkStart.bean_pause(),
            // zr z1/tr: HotSpot 25 ZGC's spelling (it never says
            // `Allocation Failure`); the start substitutes the heap's rule.
            Some((BackendBeanShape::Zgc, "Allocation Rate"))
        );
        for k in [
            NonCollectionPause::FrameTrace,
            NonCollectionPause::HeapDump,
            NonCollectionPause::HeapInspection,
            NonCollectionPause::LoopExit,
            NonCollectionPause::GenInitialMark,
            NonCollectionPause::GenRemark,
        ] {
            assert_eq!(k.bean_pause(), None, "{k:?}");
        }
    }

    /// gen r5w3/obs7: HotSpot Serial's `gc,heap` lines from the pool
    /// snapshots at the two edges of a collection: `DefNew` is eden +
    /// survivor, `From` the survivor pool, all in truncated KiB.
    #[test]
    fn xlog_heap_lines_have_serials_shape() {
        use cratonvm_gc::gc_metrics::{
            serial_pool_managers, JmxPoolUsage, JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED,
        };
        const K: u64 = 1024;
        let pool = |name, used, committed| JmxPoolUsage {
            name,
            init: None,
            used,
            committed,
            max: None,
            collection_used: None,
            managers: serial_pool_managers(name),
        };
        let before = [
            pool(JMX_POOL_EDEN, 17_472 * K + 5, 17_472 * K),
            pool(JMX_POOL_SURVIVOR, 0, 0),
            pool(JMX_POOL_TENURED, 0, 43_712 * K),
        ];
        let after = [
            pool(JMX_POOL_EDEN, 0, 17_472 * K),
            pool(JMX_POOL_SURVIVOR, 2_176 * K, 2_176 * K),
            pool(JMX_POOL_TENURED, 3_000 * K, 43_712 * K),
        ];
        let lines = render_xlog_heap_lines(3, &before, &after).expect("Serial's pools");
        assert_eq!(
            lines[0],
            "GC(3) DefNew: 17472K(17472K)->2176K(19648K) Eden: 17472K(17472K)->0K(17472K) \
             From: 0K(0K)->2176K(2176K)"
        );
        assert_eq!(lines[1], "GC(3) Tenured: 0K(43712K)->3000K(43712K)");
        // Not Serial's pools (G1's): no lines.
        assert!(render_xlog_heap_lines(3, &before[..1], &after).is_none());
    }

    /// gcg g1/c: survivors that are not a KiB multiple no longer print
    /// `DefNew` one KiB above `Eden + From` (the vf2 r2-2 sanity check).
    #[test]
    fn gcg_g1c_xlog_heap_line_defnew_is_eden_plus_from_in_kib() {
        use cratonvm_gc::gc_metrics::{
            serial_pool_managers, JmxPoolUsage, JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED,
        };
        const K: u64 = 1024;
        let pool = |name, used, committed| JmxPoolUsage {
            name,
            init: None,
            used,
            committed,
            max: None,
            collection_used: None,
            managers: serial_pool_managers(name),
        };
        let surv = 57_000;
        let before = [
            pool(JMX_POOL_EDEN, 10 * K * K, 12 * K * K),
            pool(JMX_POOL_SURVIVOR, 0, 0),
            pool(JMX_POOL_TENURED, 0, 43_712 * K),
        ];
        let after = [
            pool(JMX_POOL_EDEN, 0, 12 * K * K - surv),
            pool(JMX_POOL_SURVIVOR, surv, surv),
            pool(JMX_POOL_TENURED, 0, 43_712 * K),
        ];
        let lines = render_xlog_heap_lines(3, &before, &after).expect("Serial's pools");
        assert_eq!(
            lines[0],
            "GC(3) DefNew: 10240K(12288K)->55K(12288K) Eden: 10240K(12288K)->0K(12232K) \
             From: 0K(0K)->55K(56K)"
        );
    }

    #[test]
    fn xlog_cpu_line_has_hotspots_shape() {
        assert_eq!(
            render_xlog_cpu_line(7, 0.013, 0.0049, 9_876_543),
            "GC(7) User=0.01s Sys=0.00s Real=0.01s"
        );
        assert_eq!(
            render_xlog_cpu_line(0, -0.5, 1.5, 1_234_000_000),
            "GC(0) User=0.00s Sys=1.50s Real=1.23s"
        );
    }

    #[test]
    fn system_gc_is_reported_with_hotspots_cause_string() {
        assert_eq!(GcDoor::SystemGc.cause(), "System.gc()");
        let line = render_xlog_gc_line(0, "Pause Full", GcDoor::SystemGc.cause(), 0, 0, 0, 999);
        assert_eq!(line, "GC(0) Pause Full (System.gc()) 0M->0M(0M) 0.001ms");
    }

    /// gci i2/oo: `jcmd GC.run` is HotSpot's `_dcmd_gc_run`
    /// (`GCCause::to_string`: `Diagnostic Command`), a full collection, with a
    /// space-free token of its own on the `[GC] pause:` line.
    #[test]
    fn gci_i2oo_diagnostic_command_has_hotspots_cause_string() {
        assert_eq!(GcDoor::DiagnosticCommand.cause(), "Diagnostic Command");
        let line = render_xlog_gc_line(
            2,
            "Pause Full",
            GcDoor::DiagnosticCommand.cause(),
            0,
            0,
            0,
            999,
        );
        assert_eq!(line, "GC(2) Pause Full (Diagnostic Command) 0M->0M(0M) 0.001ms");
        let facts = PauseSafepointFacts {
            ttsp_ns: 0,
            straggler: None,
            xt_post_quota_ns: 0,
            xt_pass_ns: 0,
            xt: (0, 0, 0, 0, 0, 0),
        };
        assert!(render_pause_line(5, GcDoor::DiagnosticCommand, facts, 0).contains(" door=dcmd "));    }

    /// gci i9/dc (`gci-i8rv-dcmd-live-walk-on-an-idle-vm-FIXED-20261017` fix 2):
    /// the live-only walks' collections carry HotSpot's own causes
    /// (`GCCause::to_string`: `_heap_inspection`, `_heap_dump`), not `GC.run`'s,
    /// and door tokens of their own that alias no non-collection pause.
    #[test]
    fn gci_i9dc_live_walk_collections_have_hotspots_cause_strings() {
        assert_eq!(GcDoor::HeapInspection.cause(), "Heap Inspection Initiated GC");
        assert_eq!(GcDoor::HeapDump.cause(), "Heap Dump Initiated GC");
        let line = render_xlog_gc_line(
            3,
            "Pause Full",
            GcDoor::HeapInspection.cause(),
            0,
            0,
            0,
            999,
        );
        assert_eq!(line, "GC(3) Pause Full (Heap Inspection Initiated GC) 0M->0M(0M) 0.001ms");
        let facts = PauseSafepointFacts {
            ttsp_ns: 0,
            straggler: None,
            xt_post_quota_ns: 0,
            xt_pass_ns: 0,
            xt: (0, 0, 0, 0, 0, 0),
        };
        let inspect = render_pause_line(6, GcDoor::HeapInspection, facts, 0);
        assert!(inspect.contains(" door=heap-inspection-gc "), "{inspect}");
        let dump = render_pause_line(7, GcDoor::HeapDump, facts, 0);
        assert!(dump.contains(" door=heap-dump-gc "), "{dump}");
        for k in [NonCollectionPause::HeapInspection, NonCollectionPause::HeapDump] {
            assert_ne!(k.token(), "heap-inspection-gc");
            assert_ne!(k.token(), "heap-dump-gc");
        }
    }

    /// gcf f4/r: HotSpot 25.0.3's `gc,metaspace` line without a class space
    /// (`GC(0) Metaspace: 4137K(4224K)->4137K(4224K)` there), committed = used
    /// here, truncating K.
    #[test]
    fn gcf_f4r_metaspace_line_is_hotspots_no_class_space_shape() {
        use crate::vm::vm_init::MetaspaceStats;
        assert_eq!(
            render_xlog_metaspace_line(
                3,
                &MetaspaceStats::flat(96 * 1024 + 1023),
                &MetaspaceStats::flat(97 * 1024)
            ),
            "GC(3) Metaspace: 96K(96K)->97K(97K)"
        );
        assert_eq!(
            render_xlog_metaspace_line(0, &MetaspaceStats::flat(0), &MetaspaceStats::flat(0)),
            "GC(0) Metaspace: 0K(0K)->0K(0K)"
        );
    }

    /// gci i33/ms: with a compressed class space, HotSpot 25's
    /// `print_metaspace_change` -- `Metaspace`, `NonClass`, `Class`, each
    /// `usedK(committedK)->usedK(committedK)`.
    #[test]
    fn gci_i33ms_metaspace_line_has_the_class_space() {
        use crate::vm::vm_init::MetaspaceStats;
        const K: u64 = 1024;
        let at = |used: u64, committed: u64, class: (u64, u64)| MetaspaceStats {
            used,
            committed,
            reserved: 1_114_112 * K,
            class_space: Some((class.0, class.1, 1_048_576 * K)),
        };
        let before = at(4_137 * K + 5, 4_224 * K, (345 * K, 384 * K));
        let after = at(4_140 * K, 4_224 * K, (346 * K, 384 * K));
        assert_eq!(
            render_xlog_metaspace_line(0, &before, &after),
            "GC(0) Metaspace: 4137K(4224K)->4140K(4224K) NonClass: 3792K(3840K)->3794K(3840K) Class: 345K(384K)->346K(384K)"
        );
        // A before without a class space reads its class part as 0.
        assert!(render_xlog_metaspace_line(1, &MetaspaceStats::flat(K), &after)
            .contains(" Class: 0K(0K)->346K(384K)"));
    }

    /// gcg g3/x: HotSpot 25.0.3 Serial's debug `Heap Before GC` / `Metaspace
    /// Before GC` blocks (the measured `GC(0)` of `-Xlog:gc*=debug`, mode
    /// `sysgc` of `GcgX3XlogProbe`), and `invocations=` from the beans.
    #[test]
    fn gcg_g3x_heap_debug_blocks_are_hotspot_serials_text() {
        use crate::runtime::unified_logging::{ExitSpace, SerialExitHeap};
        use cratonvm_gc::gc_metrics::{
            GcBeanSnapshot, JmxCollector, JmxPoolUsage, JMX_SERIAL_OLD_COLLECTOR,
            JMX_SERIAL_YOUNG_COLLECTOR,
        };
        let s = |capacity: u64, used: u64, bounds: [u64; 3]| ExitSpace {
            capacity,
            used,
            bounds,
        };
        let heap = SerialExitHeap {
            young: s(
                19648 * 1024,
                9269 * 1024,
                [0x17535600000, 0x17536b50000, 0x17536b50000],
            ),
            eden: s(
                17472 * 1024,
                0x90d730,
                [0x17535600000, 0x17535f0d730, 0x17536710000],
            ),
            from: s(
                2176 * 1024,
                0,
                [0x17536710000, 0x17536710000, 0x17536930000],
            ),
            to: s(
                2176 * 1024,
                0,
                [0x17536930000, 0x17536930000, 0x17536b50000],
            ),
            old: s(
                43712 * 1024,
                1557 * 1024,
                [0x17536b50000, 0x17539600000, 0x17539600000],
            ),
            the: s(
                43712 * 1024,
                0x185738,
                [0x17536b50000, 0x17536cd5738, 0x17539600000],
            ),
            metaspace: (92 * 1024, 92 * 1024, 92 * 1024),
            class_space: None,
        };
        let (h, m) = render_xlog_heap_debug_blocks(0, "Before", (0, 0), &heap);
        assert_eq!(
            h,
            [
                "GC(0) Heap Before GC invocations=0 (full 0):",
                "GC(0)  DefNew     total 19648K, used 9269K [0x0000017535600000, 0x0000017536b50000, 0x0000017536b50000)",
                "GC(0)   eden space 17472K,  53% used [0x0000017535600000, 0x0000017535f0d730, 0x0000017536710000)",
                "GC(0)   from space 2176K,   0% used [0x0000017536710000, 0x0000017536710000, 0x0000017536930000)",
                "GC(0)   to   space 2176K,   0% used [0x0000017536930000, 0x0000017536930000, 0x0000017536b50000)",
                "GC(0)  Tenured    total 43712K, used 1557K [0x0000017536b50000, 0x0000017539600000, 0x0000017539600000)",
                "GC(0)   the  space 43712K,   3% used [0x0000017536b50000, 0x0000017536cd5738, 0x0000017539600000)",
            ]
        );
        assert_eq!(
            m,
            [
                "GC(0) Metaspace Before GC invocations=0 (full 0):",
                "GC(0)  Metaspace       used 92K, committed 92K, reserved 92K",
            ]
        );
        let collector = |name: &'static str, count: u64| JmxCollector {
            name,
            count,
            time_ms: 0,
            time_ns: 0,
            pools: &[],
            action: "end of minor GC",
        };
        let snap = GcBeanSnapshot::now(
            vec![
                collector(JMX_SERIAL_YOUNG_COLLECTOR, 3),
                collector(JMX_SERIAL_OLD_COLLECTOR, 1),
            ],
            Vec::<JmxPoolUsage>::new(),
        );
        assert_eq!(serial_invocations(&snap), (4, 1));
        let (h, _) = render_xlog_heap_debug_blocks(4, "After", (5, 1), &heap);
        assert_eq!(h[0], "GC(4) Heap After GC invocations=5 (full 1):");
    }

    /// gcf f4/r: the `gc,age` block in HotSpot 25.0.3 Serial's text (the
    /// measured `GC(0)` block of `-Xlog:gc+age=trace`), empty ages skipped.
    #[test]
    fn gcf_f4r_age_lines_are_hotspot_serials_text() {
        use crate::runtime::unified_logging::LogLevel;
        let mut ages = [0u64; 16];
        ages[1] = 112_328;
        ages[3] = 1_000;
        let lines = render_xlog_age_lines(0, 1_114_112, 15, 15, &ages);
        assert_eq!(
            lines,
            vec![
                (
                    LogLevel::Debug,
                    "GC(0) Desired survivor size 1114112 bytes, new threshold 15 (max threshold 15)"
                        .to_string()
                ),
                (LogLevel::Trace, "GC(0) Age table:".to_string()),
                (
                    LogLevel::Trace,
                    "GC(0) - age   1:     112328 bytes,     112328 total".to_string()
                ),
                (
                    LogLevel::Trace,
                    "GC(0) - age   3:       1000 bytes,     113328 total".to_string()
                ),
            ]
        );
    }

    /// gcg g5/yg (vg3 `x3_heapdbg_full`): the `gc,age` block prints on a
    /// young pause only, never on a `Pause Full`, as on HotSpot Serial.
    #[test]
    fn gcg_g5yg_age_lines_print_on_young_pauses_only() {
        assert!(xlog_age_lines_printed_for(false));
        assert!(!xlog_age_lines_printed_for(true));
    }

    /// gcg g4/yg (review finding 8): the `gc,cpu` clock exists on Windows as
    /// on Unix, and it does not run backwards across a busy loop.
    #[cfg(any(unix, windows))]
    #[test]
    fn gcg_g4yg_process_cpu_times_exist_on_windows_and_unix() {
        let (u0, s0) = process_cpu_secs().expect("process CPU times");
        let mut x = 0u64;
        for i in 0..5_000_000u64 {
            x = x.wrapping_mul(31).wrapping_add(i);
        }
        std::hint::black_box(x);
        let (u1, s1) = process_cpu_secs().expect("process CPU times");
        assert!(u0 >= 0.0 && s0 >= 0.0, "({u0}, {s0})");
        assert!(u1 >= u0 && s1 >= s0, "({u0}, {s0}) -> ({u1}, {s1})");
        assert!(
            render_xlog_cpu_line(4, u1 - u0, s1 - s0, 1_000_000).starts_with("GC(4) User="),
            "the line renders"
        );
    }

    /// gcg h1/ob (`gcg-g3x-xlog-escalated-young-collection-prints-one-pause-full`):
    /// the young half of an escalated pause ends with the pause's young pools
    /// and the old generation as the major's decision saw it; the full half
    /// starts there. The two `gc` lines then read as HotSpot Serial's pair.
    #[test]
    fn gch_h1ob_escalation_mid_figures_replace_only_the_tenured_share() {
        use cratonvm_gc::gc_metrics::{
            serial_pool_managers, JmxPoolUsage, JMX_POOL_EDEN, JMX_POOL_SURVIVOR, JMX_POOL_TENURED,
        };
        const K: u64 = 1024;
        const M: usize = 1024 * 1024;
        let pool = |name, used, committed| JmxPoolUsage {
            name,
            init: None,
            used,
            committed,
            max: None,
            collection_used: None,
            managers: serial_pool_managers(name),
        };
        let before = [
            pool(JMX_POOL_EDEN, 17_472 * K, 17_472 * K),
            pool(JMX_POOL_SURVIVOR, 0, 0),
            pool(JMX_POOL_TENURED, 34_490 * K, 43_712 * K),
        ];
        let after = [
            pool(JMX_POOL_EDEN, 0, 17_472 * K),
            pool(JMX_POOL_SURVIVOR, 2_176 * K, 2_176 * K),
            pool(JMX_POOL_TENURED, 20_000 * K, 43_712 * K),
        ];
        let mid = escalation_mid_pools(&after, 43_708 * K);
        assert_eq!(mid[0], after[0], "eden as the pause left it");
        assert_eq!(mid[1], after[1], "survivors as the pause left them");
        assert_eq!((mid[2].used, mid[2].committed), (43_708 * K, 43_712 * K));
        // A mark above the end's commit lifts the commit (`used <= committed`).
        assert_eq!(
            escalation_mid_pools(&after, 50_000 * K)[2].committed,
            50_000 * K
        );
        let young = render_xlog_heap_lines(48, &before, &mid).expect("Serial's pools");
        assert_eq!(young[1], "GC(48) Tenured: 34490K(43712K)->43708K(43712K)");
        let full = render_xlog_heap_lines(49, &mid, &after).expect("Serial's pools");
        assert_eq!(full[1], "GC(49) Tenured: 43708K(43712K)->20000K(43712K)");
        // The heap figure: the end's young share plus the mark's old share.
        let heap_after = (2_176 * K + 20_000 * K) as usize;
        let heap_mid = escalation_mid_heap_used(heap_after, &after, 43_708 * K);
        assert_eq!(heap_mid as u64, 2_176 * K + 43_708 * K);
        assert_eq!(
            render_xlog_gc_line(
                48,
                "Pause Young",
                "Allocation Failure",
                50 * M,
                heap_mid,
                61 * M,
                7_320_000
            ),
            "GC(48) Pause Young (Allocation Failure) 50M->44M(61M) 7.320ms"
        );
        assert_eq!(escalation_mid_invocations((48, 1)), (49, 1));
    }

    /// gcg h1/ob: the debug `Heap After GC` block of the young half keeps the
    /// end's spaces with the tenured space at the mark and the metaspace as
    /// the pause found it.
    #[test]
    fn gch_h1ob_escalation_mid_exit_heap_moves_the_tenured_space_only() {
        let mut after = crate::runtime::unified_logging::SerialExitHeap::default();
        after.eden.used = 5;
        after.old.used = 100;
        after.the.used = 100;
        after.the.bounds = [0x1000, 0x1064, 0x2000];
        after.metaspace = (7, 7, 7);
        let mid = escalation_mid_exit_heap(&after, 300, Some((9, 9, 9)));
        assert_eq!(mid.eden, after.eden);
        assert_eq!((mid.old.used, mid.the.used), (300, 300));
        assert_eq!(mid.the.bounds, [0x1000, 0x1000 + 300, 0x2000]);
        assert_eq!(mid.metaspace, (9, 9, 9));
        assert_eq!(
            escalation_mid_exit_heap(&after, 300, None).metaspace,
            (7, 7, 7)
        );
    }

    /// gcg h1/ob: the `Copy` notification of an escalated pause ends at the
    /// mark with the young half's pools; the `MarkSweepCompact` one starts
    /// there with them; any other bean's record is untouched.
    #[test]
    fn gch_h1ob_escalated_notifications_split_at_the_mark() {
        use cratonvm_gc::gc_metrics::{
            GcNotificationRecord, JMX_SERIAL_OLD_COLLECTOR, JMX_SERIAL_YOUNG_COLLECTOR,
        };
        let t0 = std::time::Instant::now();
        let mark = t0 + std::time::Duration::from_millis(3);
        let end = t0 + std::time::Duration::from_millis(10);
        let rec = |collector| GcNotificationRecord {
            collector,
            action: "x",
            cause: "Allocation Failure",
            id: 1,
            start: t0,
            end,
            timestamp_ms: 100,
            before: Vec::new(),
            after: Vec::new(),
        };
        let mid = escalation_mid_pools(&[], 0);
        let mut recs = vec![
            rec(JMX_SERIAL_YOUNG_COLLECTOR),
            rec(JMX_SERIAL_OLD_COLLECTOR),
            rec("Other"),
        ];
        split_escalated_notifications(&mut recs, mark, &mid);
        assert_eq!((recs[0].start, recs[0].end), (t0, mark));
        assert_eq!((recs[1].start, recs[1].end), (mark, end));
        assert_eq!((recs[2].start, recs[2].end), (t0, end));
        assert_eq!(
            recs[0].timestamp_ms, 93,
            "the young stamp moves back with its end"
        );
        assert_eq!((recs[1].timestamp_ms, recs[2].timestamp_ms), (100, 100));
    }

    /// gcg h1/ob: one `getrusage` pair brackets an escalated pause; its two
    /// `gc,cpu` lines share the delta by wall time and sum to it.
    #[test]
    fn gch_h1ob_cpu_delta_splits_by_wall_time() {
        let ((yu, ys), (fu, fs)) = split_cpu_delta((0.4, 0.2), 1_000, 4_000);
        assert!((yu - 0.1).abs() < 1e-9 && (ys - 0.05).abs() < 1e-9);
        assert!((yu + fu - 0.4).abs() < 1e-9 && (ys + fs - 0.2).abs() < 1e-9);
        assert_eq!(split_cpu_delta((1.0, 1.0), 0, 0).0, (0.0, 0.0));
    }


    /// gch h2/ob: the delivery-failure line names the bean, the action and
    /// what failed, as one space-free token per key.
    #[test]
    fn gch_h2ob_gc_notification_failure_line_is_scrapeable() {
        let line = render_gc_notification_failure(
            3,
            "G1 Young Generation",
            "end of minor GC",
            "System.gc()",
            "java.lang.IllegalStateException: no builder",
        );
        assert_eq!(
            line,
            "[GC] gc_notification_failed: n=3 collector=G1_Young_Generation \
             action=end_of_minor_GC cause=System.gc() \
             error=java.lang.IllegalStateException:_no_builder"
        );
    }

    /// gch h3/os: a thrown delivery names its class and, when it has one, its
    /// message on one line, cut to the limit.
    #[test]
    fn gch_h3os_thrown_delivery_error_carries_its_message() {
        let ae = "java.lang.AssertionError";
        assert_eq!(thrown_error_text(ae, None), ae);
        assert_eq!(thrown_error_text(ae, Some("")), ae);
        assert_eq!(
            thrown_error_text(
                "java.lang.NoClassDefFoundError",
                Some("Foo$$Lambda/0x1\nat x")
            ),
            "java.lang.NoClassDefFoundError:Foo$$Lambda/0x1 at x"
        );
        let long = "m".repeat(GC_NOTIFICATION_FAILURE_MESSAGE_CHARS + 40);
        let text = thrown_error_text("E", Some(&long));
        assert_eq!(
            text.len(),
            "E:".len() + GC_NOTIFICATION_FAILURE_MESSAGE_CHARS
        );
        // The rendered line keeps the value one token.
        let line =
            render_gc_notification_failure(1, "Copy", "end of minor GC", "System.gc()", &text);
        assert_eq!(line.split(' ').count(), 7, "{line}");
    }
