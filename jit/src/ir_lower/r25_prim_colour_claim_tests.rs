use super::*;
use crate::ir::IrBuilder;
use cratonvm_types::flags::with_thread_overrides;


/// `static int rowsum(int[][] a, int i, int n)` (the r16w7 module's OSR
/// shape): a reference parameter, an `aaload` and `int` arithmetic, so its
/// plan has colours of both classes.
const ROWSUM: [u8; 30] = [
    0x03, 0x3e, 0x03, 0x36, 0x04, 0x15, 0x04, 0x1c, 0xa2, 0x00, 0x12, 0x1d, 0x2a, 0x1b, 0x32,
    0x15, 0x04, 0x2e, 0x60, 0x3e, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xee, 0x1d, 0xac, 0, 0,
];

fn fib(n: i64) -> i64 {
    if n < 2 {
        n
    } else {
        fib(n - 1) + fib(n - 2)
    }
}


/// A hand-built plan: colours 0 and 2 are `Prim`, 1 and 3 `Ref` (node 4
/// shares colour 1). Grouped, the `Prim` colours are 0 and 1 and the `Ref`
/// ones 2 and 3, each group in its old order; classes, ranges and sharing
/// are untouched; a second call moves nothing; a single-class plan is left
/// as it is.
#[test]
fn r25_grouping_numbers_every_prim_colour_first() {
    let r = |lo: usize, hi: usize| Some(LiveRange { lo, hi });
    let mut plan = SlotPlan {
        node_color: vec![Some(0), Some(1), Some(2), Some(3), Some(1), None],
        range: vec![r(0, 9), r(0, 2), r(1, 9), r(3, 9), r(4, 9), None],
        class: vec![
            Some(SlotClass::Prim),
            Some(SlotClass::Ref),
            Some(SlotClass::Prim),
            Some(SlotClass::Ref),
            Some(SlotClass::Ref),
            None,
        ],
        slots: 4,
        peak_live: 4,
        sr_field_values: Vec::new(),
        pos_of: vec![None; 6],
        phi_code_unused: vec![false; 6],
        colour_range: Vec::new(),
        chain_ranged: 0,
        snapshot_ranged: None,
    };
    assert!(plan.group_prim_colours_first(), "colours move");
    assert_eq!(
        plan.node_color,
        vec![Some(0), Some(2), Some(1), Some(3), Some(2), None]
    );
    assert_eq!(plan.prim_colors(), vec![0, 1], "one run at the shallow end");
    assert_eq!(plan.slots, 4, "the frame is not resized");
    assert_eq!(plan.class[0], Some(SlotClass::Prim));
    assert_eq!(plan.range[4], r(4, 9));
    assert!(!plan.group_prim_colours_first(), "idempotent");

    let mut all_prim = SlotPlan {
        node_color: vec![Some(0), Some(1)],
        range: vec![r(0, 3), r(1, 3)],
        class: vec![Some(SlotClass::Prim), Some(SlotClass::Prim)],
        slots: 2,
        peak_live: 2,
        sr_field_values: Vec::new(),
        pos_of: vec![None; 2],
        phi_code_unused: vec![false; 2],
        colour_range: Vec::new(),
        chain_ranged: 0,
        snapshot_ranged: None,
    };
    assert!(!all_prim.group_prim_colours_first(), "one class: nothing to group");
    assert_eq!(all_prim.node_color, vec![Some(0), Some(1)]);
}

/// On a real plan the renumbered colouring passes `verify_slot_colouring`
/// (classes never mixed, no live pair sharing), every `Prim` colour sits
/// before every rootable one, and two nodes share a colour after exactly
/// when they shared one before.
#[test]
fn r25_a_grouped_real_plan_still_verifies() {
    let mut builder = IrBuilder::new(3, 5);
    builder.set_param_types(&[IrType::Ref, IrType::Int, IrType::Int]);
    let graph = builder.build(&ROWSUM, 28).expect("rowsum builds");
    let schedule = crate::ir_schedule::schedule(&graph);
    let old = plan_slots(&graph, &schedule, None);
    let mut new = plan_slots(&graph, &schedule, None);
    new.group_prim_colours_first();
    verify_slot_colouring(&graph, &schedule, &new).expect("the grouped plan is sound");
    assert_eq!(new.slots, old.slots);
    let prim = new.prim_colors();
    assert_eq!(prim.len(), old.prim_colors().len());
    // Cast: a colour count, far below `u32::MAX`.
    assert_eq!(prim, (0..prim.len() as u32).collect::<Vec<u32>>());
    for a in 0..graph.nodes.len() {
        assert_eq!(new.class[a], old.class[a], "n{a}'s class");
        assert_eq!(new.node_color[a].is_some(), old.node_color[a].is_some());
        for b in (a + 1)..graph.nodes.len() {
            let shared =
                |p: &SlotPlan| p.node_color[a].is_some() && p.node_color[a] == p.node_color[b];
            assert_eq!(shared(&new), shared(&old), "n{a} / n{b}");
        }
    }
}


// Executed: recursive `fib` (every call a fresh frame through the
// prologue under test) answers under the claim, publishes it -- strictly
// ascending, inside the colour band, equal to every map's
// `non_oop_stack_slots` and disjoint from every map's named slots -- and
// publishes nothing with the switch off.
