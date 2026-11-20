use super::*;

fn source() -> String {
    super::GC_AND_ALLOC_SRC.replace("\r\n", "\n")
}

fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    // `\u{7d}` is the closing brace: a bare one in a literal would end this
    // module early for the production-panic scanner, which counts braces.
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

fn at(b: &str, needle: &str) -> usize {
    b.find(needle)
        .unwrap_or_else(|| panic!("`{needle}` not found"))
}

/// HotSpot's G1 answers `System.gc()` with a stop-the-world full
/// collection unless `-XX:+ExplicitGCInvokesConcurrent`.
#[test]
fn only_explicit_gc_invokes_concurrent_keeps_the_forced_cycle_concurrent() {
    use crate::config::ExplicitGcPolicy;
    assert!(g1_force_full_cycle_is_stw(ExplicitGcPolicy::Full));
    assert!(g1_force_full_cycle_is_stw(ExplicitGcPolicy::Disabled));
    assert!(!g1_force_full_cycle_is_stw(ExplicitGcPolicy::Concurrent));
    assert!(g1_force_full_cycle_is_stw(ExplicitGcPolicy::default()));
}

/// The whole cycle is one pause: the mark opens, the skip set lands
/// before any root, the roots are marked, and the remark drain, reference
/// processing and cleanup run before the world is released.
#[test]
fn the_stw_full_cycle_marks_processes_and_cleans_up_inside_one_pause() {
    let src = source();
    let b = body(&src, "fn g1_stw_full_mark_cycle(");
    let take = at(&b, "stw_take_over_and_wait(");
    let timer = at(&b, "NonCollectionPause::G1Remark)");
    // Merge of origin/dev (2026-10-15): gci i14/gq G-4 publishes the
    // sorted skip set inside the cycle's start.
    let sorted = at(&b, "ref_objs.sort_unstable()");
    let start = at(
        &b,
        "g1_start_concurrent_mark_with_reference_skip(&stw, &ref_objs)",
    );
    let skip = start;
    let roots = at(&b, "collect_roots(shared, thread)");
    let fz = at(&b, "append_pending_finalizer_mark_roots(shared, &mut snapshot_roots)");
    let frozen = at(&b, "frozen_peer_referents(shared, &xt_roots, &ref_objs)");
    let mark = at(&b, "g1_mark_roots(&stw, &all_roots)");
    let refs = at(&b, "g1_remark_reference_pass(shared, is_live)");
    let remark = at(
        &b,
        "g1_final_remark_and_cleanup(&stw, &all_roots, Some(&mut process))",
    );
    let sweep = at(&b, "sweep_address_keyed_tables(shared)");
    let seal = at(&b, "non_collection_pause_seal(&mut pause_timer)");
    let resume = at(&b, "retire_skip_spans_and_resume(shared, taken)");
    let release = at(&b, ".complete_gc(");
    let finish = at(&b, "non_collection_pause_finish(pause_timer)");
    assert!(take < timer && timer < sorted);
    assert!(sorted < start && start <= skip && skip < roots);
    assert!(roots < fz && fz < mark);
    assert!(roots < frozen && frozen < mark && mark < refs && refs < remark);
    assert!(remark < sweep && sweep < seal && seal < resume);
    assert!(resume < release && release < finish);
    // One pause: one take-over, one release, nothing in between.
    assert_eq!(b.matches("stw_take_over_and_wait(").count(), 1);
    assert_eq!(b.matches("pause_guard.release()").count(), 1);
}

/// `g1_force_full_cycle` starts its fresh cycle through the STW door
/// when the policy says so, and counts it finished.
///
/// gci i21/fx: the loop lives in `g1_force_full_cycle_with` since gci
/// i14/ox (the public door is its `None`-hook wrapper, pinned by the
/// gci i14/ox tests' `plain` check), so the order is read there.
#[test]
fn the_forced_cycle_takes_the_stw_door_before_the_concurrent_one() {
    let src = source();
    let b = body(&src, "fn g1_force_full_cycle_with(");
    let gate = at(
        &b,
        "if g1_force_full_cycle_is_stw(shared.config.explicit_gc)",
    );
    let stw = at(&b, "g1_stw_full_mark_cycle(shared, thread)");
    let finished = stw + at(&b[stw..], "MarkDoorOutcome::Finished");
    let concurrent = at(&b, "g1_concurrent_mark_cycle(shared, thread);");
    assert!(gate < stw && stw < finished && finished < concurrent);
}

/// Both remarks run the same reference pass. Merge of origin/dev
/// (2026-10-15): the concurrent remark keeps its steps inline (the gci
/// i15/vh judge and i8/jf split sit between them, and
/// `thread_registry.rs`'s pins read them there), so both bodies are
/// checked for every step instead.
#[test]
fn both_g1_remarks_share_one_reference_pass() {
    let src = source();
    let remark = body(&src, "fn g1_final_remark_cleanup(");
    let stw = body(&src, "fn g1_stw_full_mark_cycle(");
    assert!(stw.contains("g1_remark_reference_pass(shared, is_live)"));
    let pass = body(&src, "fn g1_remark_reference_pass(");
    for term in [
        "g1_remark_mark_inherited_tl_values(shared, is_live)",
        "g1_remark_close_metadata_rows(shared, is_live)",
        "g1_remark_process_references(shared, is_live)",
        "g1_remark_mark_kept_closure(shared, &keep)",
        "clear_weak_global_refs_unmarked(shared, is_live, &keep)",
        "g1_remark_sweep_inherited_tl_buckets(shared, is_live)",
    ] {
        assert!(pass.contains(term), "the reference pass lost `{term}`");
        assert!(remark.contains(term), "the concurrent remark lost `{term}`");
    }
    // gci i20/mr finding 3: and in ONE order -- the rows closed first,
    // then the ephemeron, the reference rows, the keep closure (`=0`
    // arm), the JNI weak clear and the bucket sweep.
    for b in [&pass, &remark] {
        let order = [
            at(b, "g1_remark_close_metadata_rows(shared, is_live)"),
            at(b, "g1_remark_mark_inherited_tl_values(shared, is_live)"),
            at(b, "g1_remark_process_references(shared, is_live)"),
            at(b, "g1_remark_mark_kept_closure(shared, &keep)"),
            at(b, "clear_weak_global_refs_unmarked(shared, is_live, &keep)"),
            at(b, "g1_remark_sweep_inherited_tl_buckets(shared, is_live)"),
        ];
        for pair in order.windows(2) {
            assert!(pair[0] < pair[1], "{order:?}");
        }
    }
}

/// gci i20/mr finding 2: the STW full cycle prints the `gc+ref` block its
/// reference pass deferred, after the world is released, as the
/// concurrent remark does.
#[test]
fn gci_i20mr_the_stw_full_cycle_flushes_the_remark_reference_block() {
    let src = source();
    for sig in [
        "fn g1_stw_full_mark_cycle(",
        "fn g1_final_remark_cleanup(",
    ] {
        let b = body(&src, sig);
        let release = at(&b, ".complete_gc(");
        let finish = at(&b, "non_collection_pause_finish(pause_timer);");
        let flush = at(&b, "flush_remark_reference_lines(shared);");
        assert!(release < finish && finish < flush, "{sig}");
        assert_eq!(b.matches("flush_remark_reference_lines(").count(), 1, "{sig}");
    }
}

/// A frozen peer's `Reference` never greys its hidden referent, on either
/// object layout, and every mark-start caller hands over the SORTED
/// registered-reference set the lookup binary-searches.
#[test]
fn frozen_peer_referents_skip_the_hidden_referent_slot() {
    let src = source();
    let f = body(&src, "fn frozen_peer_referents(");
    assert!(f.contains("reference_objs.binary_search(&addr).is_ok()"));
    assert!(f.contains("layout.field_disp(0)"));
    assert!(f.contains("if Some(disp) == referent_disp"));
    assert!(f.contains("if hides_referent && i == 0"));
    let g1 = body(&src, "fn g1_concurrent_mark_cycle(");
    let sorted = at(&g1, "ref_objs.sort_unstable()");
    let call = at(&g1, "frozen_peer_referents(shared, &xt_roots, &ref_objs)");
    assert!(sorted < call);
    let zgc = body(&src, "fn zgc_concurrent_mark_cycle(");
    let sorted = at(&zgc, "objs.sort_unstable()");
    let call = at(
        &zgc,
        "frozen_peer_referents(shared, &xt_roots, &reference_objs)",
    );
    assert!(sorted < call);
}
