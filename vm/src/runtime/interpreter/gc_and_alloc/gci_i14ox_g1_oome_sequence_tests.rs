//! gci i14/ox (`gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018`
//! items 3 and 7's cost half): the G1 last-ditch rung as HotSpot's two
//! `Pause Full (G1 Compaction Pause)` collections with an attempt between
//! them, and the attempt before a fresh cycle, both opt-in.
use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_types::flags::with_thread_overrides;


type Hook<'a> = &'a mut dyn FnMut(&SharedVm, &mut JvmThread) -> bool;

fn vm(gc_algorithm: GcAlgorithm) -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm,
        ..VmConfig::default()
    })
}

fn source() -> String {
    super::GC_AND_ALLOC_SRC.replace("\r\n", "\n")
}

fn body(src: &str, sig: &str) -> String {
    let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}` not found"));
    let rest = &src[start..];
    let stop = rest
        .find("\n\u{7d}\n")
        .unwrap_or_else(|| panic!("end of `{sig}` not found"));
    rest[..stop].to_string()
}

/// Off G1 neither shape; on G1 either switch routes through the split
/// rung, and only the retry switch asks the attempt before a fresh cycle.
#[test]
fn gci_i14ox_the_ladder_shape_needs_g1_and_a_switch() {
    assert_eq!(g1_oome_ladder_shape(false, true, true), (false, false));
    assert_eq!(g1_oome_ladder_shape(true, false, false), (false, false));
    assert_eq!(g1_oome_ladder_shape(true, true, false), (true, false));
    assert_eq!(g1_oome_ladder_shape(true, false, true), (true, true));
    assert_eq!(g1_oome_ladder_shape(true, true, true), (true, true));
}


/// The second full collection gets a pause of its own only when the soft
/// rung ran none on this thread.
#[test]
fn gci_i14ox_the_second_full_pauses_only_without_a_soft_collection() {
    assert!(g1_oome_second_full_needs_own_pause(false));
    assert!(!g1_oome_second_full_needs_own_pause(true));
}

/// The hook is called at most once per cycle call and its answer is
/// returned; no hook answers `false` without calling anything.
#[test]
fn gci_i14ox_the_fresh_cycle_hook_is_called_once() {
    let shared = vm(GcAlgorithm::G1);
    let mut thread = JvmThread::new(ThreadId(0), "i14ox-hook");
    let mut none: Option<Hook<'_>> = None;
    assert!(!fresh_cycle_hook_allocated(&mut none, &shared, &mut thread));
    let mut calls = 0u32;
    let mut hook = |_: &SharedVm, _: &mut JvmThread| -> bool {
        calls += 1;
        true
    };
    {
        let mut some: Option<Hook<'_>> = Some(&mut hook as Hook<'_>);
        assert!(fresh_cycle_hook_allocated(&mut some, &shared, &mut thread));
        assert!(some.is_none(), "taken");
        assert!(!fresh_cycle_hook_allocated(&mut some, &shared, &mut thread));
    }
    assert_eq!(calls, 1);
    let mut refuse = |_: &SharedVm, _: &mut JvmThread| -> bool { false };
    let mut some: Option<Hook<'_>> = Some(&mut refuse as Hook<'_>);
    assert!(!fresh_cycle_hook_allocated(&mut some, &shared, &mut thread));
}

/// Off G1 the first half runs no cycle, never calls the hook and opens
/// no report; the split rung's attempt after it decides (one call).
#[test]
fn gci_i14ox_off_g1_the_first_half_is_a_no_op() {
    let generational = vm(GcAlgorithm::Generational);
    let mut thread = JvmThread::new(ThreadId(0), "i14ox-gen");
    let mut hook_calls = 0u32;
    let mut hook = |_: &SharedVm, _: &mut JvmThread| -> bool {
        hook_calls += 1;
        true
    };
    let (_, satisfied) =
        last_ditch_first_full(&generational, &mut thread, Some(&mut hook as Hook<'_>));
    assert!(!satisfied);
    assert_eq!(hook_calls, 0);
    assert!(generational.mem.alloc_ladder.g1_full_report.lock().is_none());
    let mut attempts = 0u32;
    let mut attempt = |_: &SharedVm, _: &mut JvmThread| -> Option<u32> {
        attempts += 1;
        Some(7)
    };
    let got = g1_last_ditch_reclaim_attempting(&generational, &mut thread, &mut attempt, true);
    assert_eq!(got, Some(7));
    assert_eq!(attempts, 1);
}

/// The rung's shape in the source: `last_ditch_reclaim` is the two
/// halves; the first half reports around the cycle and the recovery pause
/// and skips the pause when the hook allocated; the second reports around
/// the soft rung; the interpreter ladder takes the split rung only under
/// its shape; both `need_fresh` arms of the cycle ask the hook before the
/// fresh cycle.
#[test]
fn gci_i14ox_the_rung_is_two_reported_halves() {
    let src = source();
    let r = body(&src, "fn last_ditch_reclaim(");
    let first = r.find("last_ditch_first_full(shared, thread, None)").expect("first");
    let second = r
        .find("last_ditch_second_full(shared, thread, armed_before)")
        .expect("second");
    assert!(first < second);

    let f = body(&src, "fn last_ditch_first_full(");
    let open = f.find("g1_oome_full_report_begin(shared)").expect("opened");
    let cycle = f
        .find("g1_last_ditch_full_cycle_with(shared, thread, between)")
        .expect("cycle");
    let done = f
        .find("g1_full_report_marking_done(report.as_ref())")
        .expect("done");
    let recovery = f
        .find("g1_last_ditch_recovery_pause(shared, thread)")
        .expect("recovery");
    let finish = f.find("report.finish()").expect("finish");
    assert!(open < cycle && cycle < done && done < recovery && recovery < finish);
    assert!(f.contains("if !satisfied"));

    let s = body(&src, "fn last_ditch_second_full(");
    let open = s.find("g1_oome_full_report_begin(shared)").expect("opened");
    let rung = s
        .find("last_ditch_clear_soft_refs(shared, thread)")
        .expect("rung");
    let own = s
        .find("g1_oome_second_full_pause(shared, thread)")
        .expect("own pause");
    let finish = s.find("report.finish()").expect("finish");
    let major = s
        .find("last_ditch_generational_major(shared, thread, collected)")
        .expect("major");
    assert!(open < rung && rung < own && own < finish && finish < major);

    let l = body(&src, "fn last_ditch_then_second_major<T>(");
    let split = l.find("if split").expect("split arm");
    let attempting = l
        .find("g1_last_ditch_split(shared, thread, &mut *attempt, between)")
        .expect("attempting");
    let old = l
        .find("last_ditch_reclaim(shared, thread);")
        .expect("the old rung");
    assert!(split < attempting && attempting < old);

    let c = body(&src, "fn g1_force_full_cycle_with(");
    assert_eq!(
        c.matches("if fresh_cycle_hook_allocated(&mut between, shared, thread)")
            .count(),
        2
    );
    for arm in c.split("if need_fresh").skip(1) {
        let hook = arm
            .find("fresh_cycle_hook_allocated")
            .expect("hook in the arm");
        let reset = arm.find("need_fresh = false;").expect("reset in the arm");
        assert!(hook < reset);
    }
    let plain = body(&src, "fn g1_force_full_cycle(");
    // gci i28/dg: the door flag follows the hook (`false`: the policy).
    assert!(plain.contains("g1_force_full_cycle_with(shared, thread, None, false)"));
}
