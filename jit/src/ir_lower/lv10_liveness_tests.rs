use super::*;
use crate::regalloc::linear_scan_tests::{
    i65_l2_assert_live_models_equal, i65_l2_rule_grid, i71_l2_scheduled_graphs, I70_L2_CAPS,
};
use crate::regalloc::{LivenessFixedPoint, LivenessScratch, LsLegacyRun};
use cratonvm_types::flags::with_thread_overrides;


/// A small deterministic generator (xorshift64*).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// A random successor list per block: 0-3 successors (sometimes 7),
/// self loops, duplicates and out-of-range indices included.
fn random_cfg(rng: &mut Rng, nb: usize) -> Vec<Vec<usize>> {
    (0..nb)
        .map(|_| {
            let k = if rng.below(10) == 0 { 7 } else { rng.below(4) };
            (0..k).map(|_| rng.below(nb + 2)).collect()
        })
        .collect()
}

/// The scheduled graphs, plus each graph with every coloured `Int`
/// value retyped `Ref`, so the dead-home value ranges (which track
/// coloured `Ref` values only, and which no stock fixture reaches) run.
fn graphs_with_refs() -> Vec<(String, Graph, Schedule)> {
    // `Graph` and `Schedule` are not `Clone`: build the fixtures twice
    // (the builders are deterministic) and retype the second copy.
    let mut out = Vec::new();
    let again = i71_l2_scheduled_graphs();
    for ((name, graph, schedule), (_, mut retyped, retyped_schedule)) in
        i71_l2_scheduled_graphs().into_iter().zip(again)
    {
        let plan = plan_slots(&graph, &schedule, None);
        for (id, node) in retyped.nodes.iter_mut().enumerate() {
            if node.ty == IrType::Int && plan.node_color.get(id).copied().flatten().is_some() {
                node.ty = IrType::Ref;
            }
        }
        out.push((format!("{name} refs"), retyped, retyped_schedule));
        out.push((name, graph, schedule));
    }
    // JIT round 19 proposal wave 11, lane lv11 (LV10-4): a stock graph
    // with a real coloured `Ref` (an array element live into a call).
    let (graph, schedule) = super::lv11_tests::ref_array_call_loop();
    out.push(("ref_array_call_loop".to_string(), graph, schedule));
    out
}


/// i71-L2 (retire the loops): each of the four legacy copies through the
/// shared round robin is its own hand-written loop -- the same answer
/// and the same raw trace, visit count included, at every cap.
#[test]
fn lv10_the_driver_round_robin_is_each_hand_written_loop() {
    let mut cut_off = 0usize;
    for (name, graph, schedule) in graphs_with_refs() {
        // The live model.
        for rules in i65_l2_rule_grid() {
            for cap in I70_L2_CAPS {
                let what = format!("{name} live model {rules:?} cap {cap}");
                let run = |fixed_point| {
                    let mut trace = LsLegacyRun::default();
                    let model = crate::regalloc::build_live_model_legacy_with(
                        &graph,
                        &schedule,
                        rules,
                        cap,
                        fixed_point,
                        Some(&mut trace),
                    );
                    (model, trace)
                };
                let (old, old_trace) = run(LivenessFixedPoint::RoundRobin);
                let (new, new_trace) = run(LivenessFixedPoint::RoundRobinDriver);
                assert_eq!(old_trace, new_trace, "{what}");
                i65_l2_assert_live_models_equal(&old, &new, &what);
                if old_trace.ran && !old_trace.converged {
                    cut_off += 1;
                }
            }
        }
        // The slot plan's copy.
        let wants = slot_plan_wants(&graph, &schedule);
        for token in [false, true] {
            for cap in I70_L2_CAPS {
                let what = format!("{name} slot plan token {token} cap {cap}");
                let run = |fixed_point| {
                    let mut trace = LsLegacyRun::default();
                    let core = plan_slots_liveness_legacy_with(
                        &graph,
                        &schedule,
                        &wants,
                        token,
                        cap,
                        fixed_point,
                        Some(&mut trace),
                    );
                    (core, trace)
                };
                let (old, old_trace) = run(LivenessFixedPoint::RoundRobin);
                let (new, new_trace) = run(LivenessFixedPoint::RoundRobinDriver);
                assert_eq!(old, new, "{what}");
                assert_eq!(old_trace, new_trace, "{what}");
            }
        }
        // The value-read and chain-read ranges.
        let plan = plan_slots(&graph, &schedule, None);
        let span: Vec<(usize, usize)> = {
            let mut span = Vec::new();
            let mut seq = 0usize;
            for block in &schedule.blocks {
                let start = seq;
                seq += block.nodes.len() + usize::from(block.terminator.is_some());
                span.push((start, seq));
                seq += 1;
            }
            span
        };
        let candidate: Vec<bool> = plan.node_color.iter().map(Option::is_some).collect();
        for cap in I70_L2_CAPS {
            let what = format!("{name} cap {cap}");
            let value = |fixed_point| {
                let mut trace = LsLegacyRun::default();
                let out = value_use_ranges_with(
                    &graph,
                    &schedule,
                    &plan,
                    cap,
                    fixed_point,
                    Some(&mut trace),
                );
                (out, trace)
            };
            let old = value(LivenessFixedPoint::RoundRobin);
            assert_eq!(
                old,
                value(LivenessFixedPoint::RoundRobinDriver),
                "{what}: value"
            );
            let chain = |fixed_point| {
                let mut trace = LsLegacyRun::default();
                let out = chain_read_colour_ranges_with(
                    &graph,
                    &schedule,
                    &plan.pos_of,
                    &span,
                    &candidate,
                    &plan.range,
                    cap,
                    fixed_point,
                    Some(&mut trace),
                );
                (out, trace)
            };
            let old = chain(LivenessFixedPoint::RoundRobin);
            assert_eq!(
                old,
                chain(LivenessFixedPoint::RoundRobinDriver),
                "{what}: chain"
            );
        }
    }
    assert!(cut_off > 0, "some run is cut off by the cap");
}

/// The value-range copy is reached (not vacuous) on the retyped graphs.
#[test]
fn lv10_the_retyped_fixtures_reach_the_value_range_fixed_point() {
    let mut ran = 0usize;
    for (_, graph, schedule) in graphs_with_refs() {
        let plan = plan_slots(&graph, &schedule, None);
        let mut trace = LsLegacyRun::default();
        let _ = value_use_ranges_with(
            &graph,
            &schedule,
            &plan,
            SLOT_PLAN_MAX_ITERATIONS,
            LivenessFixedPoint::RoundRobin,
            Some(&mut trace),
        );
        ran += usize::from(trace.ran);
    }
    assert!(ran > 0, "the value-range fixed point runs on some fixture");
}

/// i71-L2 (one scratch): each copy through a scratch gives its answer
/// without one, for every fixed point and cap; one scratch is reused
/// across the copies of a compile and across two compiles in a row (and
/// across graphs: the key rebuilds the cached parts), with no state
/// leaking between runs.
#[test]
fn lv10_the_shared_scratch_changes_no_answer() {
    let fixed_points = [
        LivenessFixedPoint::RoundRobin,
        LivenessFixedPoint::RoundRobinDriver,
        LivenessFixedPoint::Worklist,
    ];
    // One scratch for the whole test: every graph after the first rekeys it.
    let mut scratch = LivenessScratch::new();
    for (name, graph, schedule) in graphs_with_refs() {
        let wants = slot_plan_wants(&graph, &schedule);
        let plan = plan_slots(&graph, &schedule, None);
        for round in 0..2 {
            for fixed_point in fixed_points {
                for cap in I70_L2_CAPS {
                    for token in [false, true] {
                        let what = format!(
                            "{name} round {round} {fixed_point:?} cap {cap} token {token}"
                        );
                        let run = |s: Option<&mut LivenessScratch>| {
                            let mut trace = LsLegacyRun::default();
                            let core = plan_slots_liveness_legacy_impl(
                                &graph,
                                &schedule,
                                &wants,
                                token,
                                cap,
                                fixed_point,
                                Some(&mut trace),
                                s,
                            );
                            (core, trace)
                        };
                        let old = run(None);
                        assert_eq!(old, run(Some(&mut scratch)), "{what}: slot plan");
                    }
                    for rules in i65_l2_rule_grid() {
                        let what =
                            format!("{name} round {round} {fixed_point:?} cap {cap} {rules:?}");
                        let run = |s: Option<&mut LivenessScratch>| {
                            let mut trace = LsLegacyRun::default();
                            let model = crate::regalloc::build_live_model_legacy_impl(
                                &graph,
                                &schedule,
                                rules,
                                cap,
                                fixed_point,
                                Some(&mut trace),
                                s,
                            );
                            (model, trace)
                        };
                        let (old, old_trace) = run(None);
                        let (new, new_trace) = run(Some(&mut scratch));
                        assert_eq!(old_trace, new_trace, "{what}: live model");
                        i65_l2_assert_live_models_equal(&old, &new, &what);
                    }
                    let run = |s: Option<&mut LivenessScratch>| {
                        let mut trace = LsLegacyRun::default();
                        let out = value_use_ranges_impl(
                            &graph,
                            &schedule,
                            &plan,
                            cap,
                            fixed_point,
                            Some(&mut trace),
                            s,
                        );
                        (out, trace)
                    };
                    let old = run(None);
                    assert_eq!(
                        old,
                        run(Some(&mut scratch)),
                        "{name} {fixed_point:?} cap {cap}: value"
                    );
                }
            }
        }
    }
    let stats = scratch.stats;
    assert!(stats.positions_reused > 0, "{stats:?}");
    assert!(stats.phi_edges_reused > 0, "{stats:?}");
    assert!(stats.bufs_reused > stats.bufs_fresh, "{stats:?}");
}


// ── i70-L2: the dead-ref planner's fixed points ─────────────────────

/// The reach union: the worklist is the whole-pass loop at every cap.
#[test]
fn lv10_the_reach_union_worklist_is_its_round_robin() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut multi = 0usize;
    for i in 0..400 {
        let nb = 1 + rng.below(80);
        let succ = random_cfg(&mut rng, nb);
        let words = 1 + rng.below(2);
        let init: Vec<Vec<u64>> = (0..nb)
            .map(|_| {
                (0..words)
                    .map(|_| rng.next() & rng.next() & rng.next())
                    .collect()
            })
            .collect();
        let run = |cap: usize, worklist: bool| {
            let mut reach = init.clone();
            let mut visits = 0usize;
            let ok = snapshot_reach_fixed_point(
                succ.as_slice(),
                &mut reach,
                cap,
                worklist,
                &mut visits,
            );
            (ok, reach, visits)
        };
        let (ok, _, visits) = run(10_000, false);
        assert!(ok, "cfg {i}");
        let passes = visits / nb;
        if passes > 2 {
            multi += 1;
        }
        for cap in (0..=passes + 1).chain([SLOT_PLAN_MAX_ITERATIONS]) {
            let rr = run(cap, false);
            let wl = run(cap, true);
            assert_eq!((rr.0, &rr.1), (wl.0, &wl.1), "cfg {i} cap {cap}");
            assert_eq!(rr.0, cap >= passes, "cfg {i} cap {cap} passes {passes}");
            assert!(wl.2 <= rr.2, "cfg {i} cap {cap}: visits");
        }
    }
    assert!(
        multi > 10,
        "the grid needs several passes somewhere ({multi})"
    );
}

fn random_steps(rng: &mut Rng, nb: usize, words: usize) -> Vec<Vec<LiveStep>> {
    let mut node = 0u32;
    (0..nb)
        .map(|_| {
            let len = rng.below(5);
            (0..len)
                .map(|_| {
                    node += 1;
                    LiveStep {
                        reads: (0..words)
                            .map(|_| rng.next() & rng.next() & rng.next())
                            .collect(),
                        kill: (rng.below(2) == 0).then(|| rng.below(words * 64 + 3)),
                        node: node as NodeId,
                        is_call: rng.below(3) == 0,
                    }
                })
                .collect()
        })
        .collect()
}

/// `live_colours_after_calls`: the worklist (in-place transfer) is the
/// whole-pass loop at every cap, including a successor table shorter
/// than the block list.
#[test]
fn lv10_the_live_colours_worklist_is_its_round_robin() {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for i in 0..400 {
        let nb = 1 + rng.below(60);
        let words = 1 + rng.below(2);
        let steps = random_steps(&mut rng, nb, words);
        let mut succ = random_cfg(&mut rng, nb);
        if i % 7 == 0 {
            succ.truncate(nb / 2);
        }
        let run = |cap: usize, worklist: bool| {
            let mut visits = 0usize;
            let out =
                live_colours_after_calls_with(&steps, &succ, words, cap, worklist, &mut visits);
            (out, visits)
        };
        let (full, visits) = run(10_000, false);
        assert!(full.is_some(), "cfg {i}");
        let passes = visits / nb;
        for cap in (0..=passes + 1).chain([SLOT_PLAN_MAX_ITERATIONS]) {
            let rr = run(cap, false);
            let wl = run(cap, true);
            assert_eq!(rr.0, wl.0, "cfg {i} cap {cap}");
            assert_eq!(
                rr.0.is_some(),
                cap >= passes,
                "cfg {i} cap {cap} passes {passes}"
            );
            assert!(wl.1 <= rr.1, "cfg {i} cap {cap}: visits");
        }
    }
}

/// `stale_sets_at_sites` (the forward union): the worklist is the
/// whole-pass loop at every cap, orphan blocks and out-of-range
/// predecessors included.
#[test]
fn lv10_the_stale_union_worklist_is_its_round_robin() {
    let mut rng = Rng(0x94D0_49BB_1331_11EB);
    let mut multi = 0usize;
    for i in 0..400 {
        let nb = 1 + rng.below(60);
        let words = 1 + rng.below(2);
        let stage_words = 1;
        let blocks: Vec<StaleBlock> = (0..nb)
            .map(|_| {
                let events = rng.below(5);
                let events = (0..events)
                    .map(|_| match rng.below(3) {
                        0 => StaleEvent::Site {
                            node: rng.below(1000) as NodeId,
                            dead: (0..words).map(|_| rng.next()).collect(),
                            cleans: rng.below(4) != 0,
                        },
                        1 => StaleEvent::Staged(rng.below(64)),
                        _ => StaleEvent::Def(rng.below(words * 64)),
                    })
                    .collect();
                let preds = rng.below(4);
                let predecessors = (0..preds).map(|_| rng.below(nb + 2)).collect();
                StaleBlock {
                    events,
                    predecessors,
                }
            })
            .collect();
        let all: Vec<u64> = (0..words).map(|_| rng.next()).collect();
        let run = |cap: usize, worklist: bool| {
            let mut visits = 0usize;
            let out = stale_sets_at_sites_with(
                &blocks,
                0,
                &all,
                stage_words,
                cap,
                worklist,
                &mut visits,
            );
            (out, visits)
        };
        let (full, visits) = run(10_000, false);
        assert!(full.is_some(), "cfg {i}");
        let passes = visits / nb;
        if passes > 2 {
            multi += 1;
        }
        for cap in (0..=passes + 1).chain([SLOT_PLAN_MAX_ITERATIONS]) {
            let rr = run(cap, false);
            let wl = run(cap, true);
            assert_eq!(rr.0, wl.0, "cfg {i} cap {cap}");
            assert_eq!(
                rr.0.is_some(),
                cap >= passes,
                "cfg {i} cap {cap} passes {passes}"
            );
            assert!(wl.1 <= rr.1, "cfg {i} cap {cap}: visits");
        }
    }
    assert!(
        multi > 10,
        "the grid needs several passes somewhere ({multi})"
    );
}

