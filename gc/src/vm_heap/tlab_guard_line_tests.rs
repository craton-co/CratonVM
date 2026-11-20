use super::render_tlab_guard_line;

/// The urgent rendering must keep the routine one's grep surface and add
/// to it — never replace it. A reader's `grep filler_over_object=` has to
/// match in BOTH states, or the only run that matters is the one where the
/// grep stops working.
#[test]
fn the_urgent_guard_line_extends_the_routine_one_rather_than_replacing_it() {
    let routine = "[GC] tlab-guard: filler_over_object=0 refill_over_object=0";
    assert_eq!(render_tlab_guard_line(routine, false), routine);

    let fired = "[GC] tlab-guard: filler_over_object=3 refill_over_object=0";
    let urgent = render_tlab_guard_line(fired, true);
    assert!(
        urgent.starts_with(fired),
        "the urgent line must begin with the exact routine line so the same \
         grep matches both: {urgent}"
    );
    assert!(
        urgent.len() > fired.len(),
        "a fired tripwire that renders identically to a quiet one is the \
         write-only instrument this page was filed over, one step removed"
    );
    assert!(
        urgent.contains("TRIPWIRE FIRED"),
        "the urgent marker is what a soak log greps for: {urgent}"
    );
}

/// The real formatter and this renderer must agree: `tlab.rs` owns the
/// wording of the line, `vm_heap.rs` owns only the suffix. If the census
/// ever stops producing a `tlab-guard` line, the suffix would be pinned to
/// nothing and this test says so rather than letting the summary print a
/// decorated empty string.
#[test]
fn the_renderer_is_fed_by_the_real_census_formatter() {
    let [_, guard] = cratonvm_gc_core::tlab::tlab_census_lines();
    assert!(
        guard.starts_with("[GC] tlab-guard: filler_over_object="),
        "unexpected census wording, which `print_gc_summary` prints verbatim: {guard}"
    );
    assert!(render_tlab_guard_line(&guard, true).starts_with(&guard));
}
