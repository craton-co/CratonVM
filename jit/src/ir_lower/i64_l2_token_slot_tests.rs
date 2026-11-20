use super::*;
use crate::regalloc::linear_scan_tests::nested_loop_with_a_token_read;


/// The i58-L2 shape: `stop` (a volatile `getstatic`, `Int`) is named only
/// as `burnt`'s memory token inside the inner loop. Off, its home is held
/// across that loop; on, its home range is its definition alone. Both
/// colourings verify, the value set and the pinned set are unchanged, and
/// no unpinned range grows.
#[test]
fn i64_l2_slot_plan_drops_a_token_only_read_from_the_home_range() {
    let (graph, schedule, _inner, stop) = nested_loop_with_a_token_read();
    let off = plan_slots_with_token_edges(&graph, &schedule, None, false);
    let on = plan_slots_with_token_edges(&graph, &schedule, None, true);
    verify_slot_colouring(&graph, &schedule, &off).expect("off: a sound colouring");
    verify_slot_colouring(&graph, &schedule, &on).expect("on: a sound colouring");

    let r_off = off.range[stop as usize].expect("`stop` wants a slot");
    let r_on = on.range[stop as usize].expect("`stop` wants a slot");
    assert_eq!(
        r_on.lo, r_on.hi,
        "on: no read, so the home range is the definition"
    );
    assert!(
        r_off.hi - r_off.lo > r_on.hi - r_on.lo,
        "off: the token read holds the home ({r_off:?} vs {r_on:?})"
    );
    assert!(on.peak_live <= off.peak_live);

    let has_colour =
        |p: &SlotPlan| p.node_color.iter().map(Option::is_some).collect::<Vec<_>>();
    assert_eq!(
        has_colour(&off),
        has_colour(&on),
        "the value set is unchanged"
    );
    let pinned = |p: &SlotPlan| {
        p.class
            .iter()
            .map(|c| matches!(c, Some(SlotClass::Pinned)))
            .collect::<Vec<_>>()
    };
    assert_eq!(pinned(&off), pinned(&on), "the pinned set is unchanged");
    let pinned_off = pinned(&off);
    for id in 0..graph.nodes.len() {
        if pinned_off[id] {
            continue;
        }
        if let (Some(a), Some(b)) = (off.range[id], on.range[id]) {
            assert!(
                b.lo >= a.lo && b.hi <= a.hi,
                "n{id}: the rule only removes reads ({a:?} -> {b:?})"
            );
        }
    }

    let column = token_trim_slot_column(&graph, &schedule, None);
    assert_eq!(column.words_off, off.slots);
    assert_eq!(column.words_on, on.slots);
    assert!(column.homes_trimmed >= 1, "{column:?}");
    assert!(column.ref_words_off <= column.words_off);
    assert!(column.ref_words_on <= column.words_on);
}



/// The guard's set: the token producer, once, and nothing a value slot
/// also names.
#[test]
fn i64_l2_token_only_inputs_name_the_token_producer_and_nothing_else() {
    let (graph, _, _, stop) = nested_loop_with_a_token_read();
    let burnt = graph
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::LoadStatic { .. }) && n.inputs.get(1) == Some(&stop))
        .expect("`burnt` names `stop` as its token");
    let node = &graph.nodes[burnt];
    assert_eq!(
        token_only_inputs_in(node, |_| false),
        vec![(stop, 1, false)]
    );
    assert_eq!(
        token_only_inputs_in(node, |id| id == stop),
        vec![(stop, 1, true)]
    );

    // The same producer in a value slot of the same node is a real read.
    let mut both = node.clone();
    both.inputs.push(stop);
    assert!(crate::ir::is_memory_token_slot(&both, 1));
    assert!(!crate::ir::is_memory_token_slot(&both, 2));
    assert!(token_only_inputs_in(&both, |_| false).is_empty());

    // A value op names nothing in a token slot.
    let value_reader = graph
        .nodes
        .iter()
        .find(|n| matches!(n.op, Op::Add))
        .expect("the loop adds");
    assert!(token_only_inputs_in(value_reader, |_| false).is_empty());
}


fn i64_l2_the_guard_refuses_a_read_through_a_token_slot_body() {
    let mut lo = super::tests::lowerer_with_resident_xmm(4096, None);

    lo.token_only_inputs = vec![(0, 1, true)];
    let _ = lo.resident_gpr(0);
    assert!(
        lo.take_latched_bailout().is_none(),
        "exempt: counted, not refused"
    );
    assert_eq!(lo.token_read_pinned_hits.get(), 1);
    assert_eq!(lo.token_read_refusals.get(), 0);

    lo.token_only_inputs = vec![(7, 1, false)];
    let _ = lo.slot_of_checked(0);
    let _ = lo.resident_xmm(0);
    assert!(lo.take_latched_bailout().is_none(), "n0 is not in the set");
    assert_eq!(lo.token_read_refusals.get(), 0);

    lo.token_only_inputs = vec![(0, 1, false)];
    let _ = lo.resident_xmm(0);
    let refused = lo
        .take_latched_bailout()
        .expect("a read through a token slot");
    assert!(
        matches!(
            refused.reason,
            BailoutReason::Internal(what) if what.contains("memory-token slot")
        ),
        "{refused:?}"
    );
    assert_eq!(lo.token_read_refusals.get(), 1);

    let _ = lo.slot_of_checked(0);
    assert!(
        lo.take_latched_bailout().is_some(),
        "the home funnel checks too"
    );
    lo.token_only_inputs.clear();
    let _ = lo.slot_of_checked(0);
    assert!(
        lo.take_latched_bailout().is_none(),
        "an empty set checks nothing"
    );
}
