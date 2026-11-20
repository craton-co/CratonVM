use super::*;
use crate::regalloc::linear_scan_tests::{i67_l2_graphs, I70_L2_CAPS};
use crate::regalloc::{LivenessFixedPoint, LsLegacyRun};


/// `plan_slots`' block spans (one position per node and terminator, then
/// the outgoing edge), as `value_use_ranges` numbers them.
fn i70_l2_spans(schedule: &Schedule) -> Vec<(usize, usize)> {
    let mut span = Vec::with_capacity(schedule.blocks.len());
    let mut seq = 0usize;
    for block in &schedule.blocks {
        let start = seq;
        seq += block.nodes.len() + usize::from(block.terminator.is_some());
        span.push((start, seq));
        seq += 1;
    }
    span
}

/// One legacy copy, ready to run under a fixed point and a cap: returns
/// the fixed point's trace.
pub(crate) type I70Copy<'a> = Box<dyn Fn(LivenessFixedPoint, usize) -> LsLegacyRun + 'a>;

/// The three `ir_lower` copies over one graph, their inputs (wanted
/// values, the slot plan, spans, candidates) computed once: the slot
/// plan's liveness (`plan_slots_liveness_legacy`), the value-read ranges
/// (`value_use_ranges`) and the chain-read ranges
/// (`chain_read_colour_ranges`, every coloured value a candidate).
pub(crate) fn i70_l2_ir_lower_copies<'a>(
    graph: &'a Graph,
    schedule: &'a Schedule,
) -> Vec<(&'static str, I70Copy<'a>)> {
    let wants = slot_plan_wants(graph, schedule);
    let plan = std::rc::Rc::new(plan_slots(graph, schedule, None));
    let span = i70_l2_spans(schedule);
    let candidate: Vec<bool> = plan.node_color.iter().map(Option::is_some).collect();
    let value_plan = std::rc::Rc::clone(&plan);
    let mut out: Vec<(&'static str, I70Copy<'a>)> = Vec::new();
    out.push((
        "slot plan",
        Box::new(move |fixed_point: LivenessFixedPoint, cap: usize| {
            let mut trace = LsLegacyRun::default();
            let _ = plan_slots_liveness_legacy_with(
                graph,
                schedule,
                &wants,
                false,
                cap,
                fixed_point,
                Some(&mut trace),
            );
            trace
        }),
    ));
    out.push((
        "value ranges",
        Box::new(move |fixed_point: LivenessFixedPoint, cap: usize| {
            let mut trace = LsLegacyRun::default();
            let _ = value_use_ranges_with(
                graph,
                schedule,
                &value_plan,
                cap,
                fixed_point,
                Some(&mut trace),
            );
            trace
        }),
    ));
    out.push((
        "chain ranges",
        Box::new(move |fixed_point: LivenessFixedPoint, cap: usize| {
            let mut trace = LsLegacyRun::default();
            let _ = chain_read_colour_ranges_with(
                graph,
                schedule,
                &plan.pos_of,
                &span,
                &candidate,
                &plan.range,
                cap,
                fixed_point,
                Some(&mut trace),
            );
            trace
        }),
    ));
    out
}



