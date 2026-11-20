    use super::*;

    /// THE STATE THESE TESTS EXERCISE IS PROCESS-GLOBAL, and `cargo test` runs
    /// one crate's tests as threads of one process. The refutation latch and
    /// the moving-young coverage cycle are one shared pair of globals, so
    /// `a_refutation_latches_and_does_not_clear`'s
    /// `note_coverage_oracle_refutation()` can land between this module's other
    /// test beginning a cycle and reading it back -- and `mod tests`'
    /// `beginning_a_coverage_cycle_clears_the_peer_ledger` begins a cycle of
    /// its own from a third thread.
    ///
    /// MEASURED on the build host: `assertion failed:
    /// !moving_young_coverage_incomplete()` at the FIRST assert of
    /// `a_refutation_marks_this_cycle_incomplete`, on a loaded host, in a tree
    /// where the same gate had passed the run before. The module alone passes
    /// 4 of 4 either way, which is what says race rather than defect.
    ///
    /// Serialising is the fix rather than merging the tests: they assert
    /// different properties and a reader should see which one broke.
    /// `parking_lot` is not a dependency here, and a poisoned `std` mutex would
    /// turn one failure into a cascade, so the guard is taken through
    /// `unwrap_or_else(|e| e.into_inner())`. Same shape as `types`'
    /// `arraylist_view` latch.
    pub(super) static COVERAGE_ORACLE_TEST_LATCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The latch starts clear, a refutation sets it, and it does not clear
    /// itself. One-way is the point: the refutation is about compiled code
    /// that is still in the cache, not about the moment it was observed.
    #[test]
    fn a_refutation_latches_and_does_not_clear() {
        let _serialised = COVERAGE_ORACLE_TEST_LATCH
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_coverage_oracle_refuted_for_test();
        assert!(!coverage_oracle_refuted(), "latch must start clear");
        note_coverage_oracle_refutation();
        assert!(coverage_oracle_refuted(), "a refutation must latch");
        // A later cycle that observes nothing must not un-refute.
        assert!(
            coverage_oracle_refuted(),
            "the latch must survive a cycle that saw nothing"
        );
        reset_coverage_oracle_refuted_for_test();
    }

    /// A refutation marks the CURRENT cycle incomplete too, not just later
    /// ones — the collector asking the question is mid-decision.
    #[test]
    fn a_refutation_marks_this_cycle_incomplete() {
        let _serialised = COVERAGE_ORACLE_TEST_LATCH
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_coverage_oracle_refuted_for_test();
        cratonvm_gc::gc_quiescence::begin_moving_young_coverage_cycle();
        assert!(!cratonvm_gc::gc_quiescence::moving_young_coverage_incomplete());
        note_coverage_oracle_refutation();
        assert!(
            cratonvm_gc::gc_quiescence::moving_young_coverage_incomplete(),
            "the cycle being decided must go incomplete"
        );
        assert_eq!(
            cratonvm_gc::gc_quiescence::moving_young_incomplete_reason(),
            cratonvm_gc::gc_quiescence::incomplete_reason::COVERAGE_ORACLE_REFUTED,
        );
        reset_coverage_oracle_refuted_for_test();
        cratonvm_gc::gc_quiescence::begin_moving_young_coverage_cycle();
    }

    /// The gate is inert unless something is actually doing the verifying.
    /// Default-off is what keeps this free on the production path, so a
    /// regression that turns it on by accident must fail here.
    #[test]
    fn the_gate_is_inert_by_default() {
        assert!(
            !coverage_gate_active(),
            "neither CRATONVM_DBG_VERIFY_OOP_MAPS nor the force switch is set \
             in the test environment, so the gate must not run"
        );
    }

    /// The refutation gate must watch the map-SELECTION counter, not only the
    /// two coverage ones.
    ///
    /// `WRONG_MAP` counts a live in-band oop named by some map of the method
    /// but not by the one its safepoint id selects, and the precise scan reads
    /// only the selected map — so the suppression this gate licenses would
    /// strand it. Measured on the 2026-09-02 soak, `HumongousHold` reports
    /// `while_covered=0 wrong_map=160`: the two counters this gate used to read
    /// were BOTH zero over frames holding 160 oops the precise walk cannot
    /// reach.
    ///
    /// It must read the VERIFIER-CONFIRMED subset, not the raw counter. The raw
    /// one over-reports: a slot whose value merely looks like a heap address is
    /// counted even when it is dead storage, which the precise map is right to
    /// omit. Measured 2026-09-02, the class file's own type maps returned
    /// `verifier_oop=0` over every never-mapped word on these workloads, so a
    /// gate keyed on the raw count would refuse the suppression forever on
    /// evidence that is not evidence.
    ///
    /// A source-level assertion because the gate needs live compiled frames to
    /// run: what is pinned here is that the counter appears in the decision at
    /// all, which is the thing that was missing.
    #[test]
    fn the_refutation_gate_reads_the_map_selection_counter() {
        let src = concat!(include_str!("../conservative_roots/mod.rs"), "\n", include_str!("../conservative_roots/jit_frame_chain_entry.rs"), "\n", include_str!("../conservative_roots/gc_outgoing_arg_roots_enabled.rs"), "\n", include_str!("../conservative_roots/refresh_moving_young_coverage_for.rs"), "\n", include_str!("../conservative_roots/warn_cross_thread_jit_gap.rs"), "\n", include_str!("../conservative_roots/scan_one_frame.rs"));
        let gate = src
            .split("pub fn verify_active_coverage_into")
            .nth(1)
            .expect("the gate is defined in this file");
        let body = &gate[..gate
            .find(
                "
}
",
            )
            .expect("the gate has a body")];
        for counter in [
            "NEVER_MAPPED_WHILE_COVERED",
            "NEVER_MAPPED_WHILE_SHADOW_COVERED",
            "WRONG_MAP_VERIFIER_OOP",
        ] {
            assert!(
                body.contains(counter),
                "verify_active_coverage_into must consult {counter}: a gate that \
                 skips one returns \"proof holds\" over frames the precise scan \
                 cannot cover"
            );
        }
    }
