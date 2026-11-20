use super::{
    cached_osr_body_is_despec_stale, charge_osr_guard_exit, osr_guard_exit_reason,
    osr_reentry_memo_get, osr_reentry_memo_put, osr_reentry_stamp,
};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_jit::deopt::{
    DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, ReconstructedFrame,
    ResumeSemantics,
};
use cratonvm_types::ClassId;
use std::sync::Arc;

const CLASS: &str = "r9w11/TryLoopRefl";
const METHOD: &str = "sumThrow";
const DESC: &str = "([II)J";
const KEY: &str = "r9w11/TryLoopRefl.sumThrow:([II)J";

fn point(bci: u32, reason: DeoptReason, semantics: ResumeSemantics) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason,
        action: DeoptAction::Reinterpret,
        semantics,
        speculation_id: cratonvm_jit::deopt::speculation_id(bci, reason),
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
    }
}

/// The loop header of `sumThrow`: the speculative-BCE guard and the OSR-exit
/// map at the same bci, recorded map-first as the single-pass backend does.
fn loop_header_points() -> Vec<DeoptimizationPoint> {
    vec![
        point(5, DeoptReason::OsrExit, ResumeSemantics::REEXECUTE),
        point(5, DeoptReason::BoundsCheck, ResumeSemantics::REEXECUTE),
        point(15, DeoptReason::PendingException, ResumeSemantics::RETHROW),
    ]
}

/// An artifact carrying `points`, OSR-enterable at every pc below 32, with
/// the metadata only (the `ret` body never runs).
fn body(epoch: u64, points: Vec<DeoptimizationPoint>) -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.compilation_epoch = epoch;
    cm.deopt_points = points;
    cm.compiled_via_osr = true;
    cm.osr_pc_to_native = Some(vec![0; 32]);
    cm
}

fn frame(bci: u32) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: KEY.to_string(),
        bci,
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: Vec::new(),
        semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    }
}

/// The OSR-exit test triggers make an `OsrExit` map a real exit source,
/// and the charge refuses to run under them.
fn test_triggers_armed() -> bool {
    cratonvm_jit::osr_exit_test_enabled()
        || cratonvm_jit::osr_exit_after().is_some()
        || cratonvm_jit::deopt_eager_enabled()
        || cratonvm_jit::deopt_eager_bci_override().is_some()
}

#[test]
fn a_loop_header_exit_is_the_guard_not_the_exit_map() {
    let points = loop_header_points();
    assert_eq!(
        osr_guard_exit_reason(&points, 5),
        Some(DeoptReason::BoundsCheck)
    );
    // The exceptional frame is not an ordinary exit.
    assert_eq!(osr_guard_exit_reason(&points, 15), None);
    // An exit map alone is no speculation.
    let exit_only = vec![point(5, DeoptReason::OsrExit, ResumeSemantics::REEXECUTE)];
    assert_eq!(osr_guard_exit_reason(&exit_only, 5), None);
    // The invokedynamic trap keeps its give-up-at-once policy elsewhere.
    let indy = vec![point(
        7,
        DeoptReason::UnreachedCode,
        ResumeSemantics::REEXECUTE,
    )];
    assert_eq!(osr_guard_exit_reason(&indy, 7), None);
    // Two different guards at one bci cannot be told apart.
    let two = vec![
        point(9, DeoptReason::BoundsCheck, ResumeSemantics::REEXECUTE),
        point(
            9,
            DeoptReason::ReceiverTypeChanged,
            ResumeSemantics::REEXECUTE,
        ),
    ];
    assert_eq!(osr_guard_exit_reason(&two, 9), None);
    let receiver = vec![point(
        9,
        DeoptReason::ReceiverTypeChanged,
        ResumeSemantics::REEXECUTE,
    )];
    assert_eq!(
        osr_guard_exit_reason(&receiver, 9),
        Some(DeoptReason::ReceiverTypeChanged)
    );
}

#[test]
fn a_trapped_frame_at_a_loop_header_is_charged_as_its_guard() {
    if cratonvm_jit::osr_exit_test_enabled() || cratonvm_jit::osr_exit_after().is_some() {
        return;
    }
    let cm = body(0, loop_header_points());
    // Round 11 wave 2: the shared attribution (`trap_reason_for_frame`)
    // replaced `guard_reason_for_trapped_frame`; the rule is the same.
    assert_eq!(
        crate::runtime::interpreter::trap_reason_for_frame(&cm, &[], &frame(5)),
        DeoptReason::BoundsCheck,
        "not the first-in-list OsrExit map"
    );
    // Nothing at the bci: charged as `UncommonTrap`.
    assert_eq!(
        crate::runtime::interpreter::trap_reason_for_frame(&cm, &[], &frame(40)),
        DeoptReason::UncommonTrap
    );
}

/// `TryLoopRefl`'s shape: an OSR body whose loop-header guard exits on
/// every entry. Four charged exits withdraw exactly that guard, and the
/// cached body is then stale for the OSR door.
#[test]
fn repeated_osr_guard_exits_despec_the_guard_and_stale_the_cached_body() {
    if test_triggers_armed() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let cm = body(0, loop_header_points());
    let spec = cratonvm_jit::deopt::speculation_id(5, DeoptReason::BoundsCheck);
    for n in 1..=4 {
        // Charged, so `try_osr`'s TRANSFER arm reports the entry as
        // committed rather than as a rejection (wave 4, L7).
        assert!(
            charge_osr_guard_exit(
                &shared,
                ClassId::new(0),
                CLASS,
                METHOD,
                DESC,
                &cm,
                &frame(5),
                None,
            ),
            "a failed speculation's exit is charged (n={n})"
        );
        let now = shared
            .jit
            .despec_registry
            .contains_speculation(KEY, 5, spec);
        assert_eq!(now, n >= 4, "per-bci de-spec after 4 charged exits (n={n})");
    }
    if cratonvm_jit::deopt_real_enabled() {
        assert!(
            cached_osr_body_is_despec_stale(
                &shared,
                &cm,
                &Arc::from(CLASS),
                &Arc::from(METHOD),
                &Arc::from(DESC),
            ),
            "the body compiled before the de-spec must be recompiled, not reused"
        );
    }
}

#[test]
fn exits_that_are_not_a_failed_speculation_are_not_charged() {
    if test_triggers_armed() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let before = shared.jit.deopt_log.lock().total_deopts();
    let exit_only = body(
        0,
        vec![point(5, DeoptReason::OsrExit, ResumeSemantics::REEXECUTE)],
    );
    let mut inlined = frame(5);
    inlined.caller_frames.push(frame(5));
    let guarded = body(0, loop_header_points());
    for _ in 0..6 {
        // None of these is charged, so `try_osr` keeps charging the
        // rejection budget for them: that budget is what bounds them.
        assert!(!charge_osr_guard_exit(
            &shared,
            ClassId::new(0),
            CLASS,
            METHOD,
            DESC,
            &exit_only,
            &frame(5),
            None,
        ));
        // An inlined chain's bci is a callee's.
        assert!(!charge_osr_guard_exit(
            &shared,
            ClassId::new(0),
            CLASS,
            METHOD,
            DESC,
            &guarded,
            &inlined,
            None,
        ));
        // The identity-less re-run sentinel.
        assert!(!charge_osr_guard_exit(
            &shared,
            ClassId::new(0),
            CLASS,
            METHOD,
            DESC,
            &guarded,
            &frame(u32::MAX),
            None,
        ));
    }
    assert_eq!(shared.jit.deopt_log.lock().total_deopts(), before);
    assert!(!shared.jit.despec_registry.contains(KEY, 5));
}

/// Round 11 wave 5: two guards at one bci cannot be named from the bci, so
/// such an exit used to go uncharged forever. The stash's own cause names
/// it; a cause this artifact has no point for, or one outside the two
/// charged reasons, still charges nothing.
#[test]
fn the_stashed_cause_charges_an_exit_the_bci_cannot_name() {
    if test_triggers_armed() {
        return;
    }
    use cratonvm_jit::deopt::{speculation_id, DeoptCause};
    let shared = SharedVm::new(VmConfig::default());
    let cm = body(
        0,
        vec![
            point(9, DeoptReason::BoundsCheck, ResumeSemantics::REEXECUTE),
            point(
                9,
                DeoptReason::ReceiverTypeChanged,
                ResumeSemantics::REEXECUTE,
            ),
            point(7, DeoptReason::UnreachedCode, ResumeSemantics::REEXECUTE),
        ],
    );
    let cause = |bci: u32, reason: DeoptReason| {
        Some(DeoptCause {
            reason,
            speculation_id: speculation_id(bci, reason),
        })
    };
    let before = shared.jit.deopt_log.lock().total_deopts();
    let charge = |frame_bci: u32, c: Option<DeoptCause>| {
        charge_osr_guard_exit(
            &shared,
            ClassId::new(0),
            CLASS,
            METHOD,
            DESC,
            &cm,
            &frame(frame_bci),
            c,
        );
    };
    charge(9, None);
    charge(9, cause(9, DeoptReason::NullCheck));
    charge(7, cause(7, DeoptReason::UnreachedCode));
    assert_eq!(shared.jit.deopt_log.lock().total_deopts(), before);
    for _ in 0..4 {
        charge(9, cause(9, DeoptReason::ReceiverTypeChanged));
    }
    let registry = &shared.jit.despec_registry;
    assert!(registry.contains_speculation(
        KEY,
        9,
        speculation_id(9, DeoptReason::ReceiverTypeChanged)
    ));
    assert!(!registry.contains_speculation(
        KEY,
        9,
        speculation_id(9, DeoptReason::BoundsCheck)
    ));
}

/// The re-entry memo answers with the artifact it was given while every
/// stamp holds, and with nothing as soon as one moves.
#[test]
fn the_reentry_memo_hits_only_while_its_stamps_hold() {
    let shared = SharedVm::new(VmConfig::default());
    let Some(stamp) = osr_reentry_stamp(&shared) else {
        return; // CRATONVM_JIT_OSR_REENTRY_MEMO=0
    };
    let class: Arc<str> = Arc::from(CLASS);
    let method: Arc<str> = Arc::from(METHOD);
    let desc: Arc<str> = Arc::from(DESC);
    let cid = ClassId::new(7);
    let code = 0x1000usize;
    let cm = Arc::new(body(0, loop_header_points()));
    osr_reentry_memo_put(&shared, stamp, code, cid, &method, &desc, 5, &cm);
    let get = |stamp: (u64, u64), code: usize, pc: usize| {
        osr_reentry_memo_get(&shared, stamp, code, cid, &class, &method, &desc, pc)
    };
    let hit = get(stamp, code, 5).expect("same entry, same stamps");
    assert!(Arc::ptr_eq(&hit, &cm));
    drop(hit);
    assert!(get(stamp, code, 6).is_none(), "another pc");
    assert!(get(stamp, 0x2000, 5).is_none(), "another bytecode");
    assert!(
        get((stamp.0 + 1, stamp.1), code, 5).is_none(),
        "a publication"
    );
    assert!(get((stamp.0, stamp.1 + 1), code, 5).is_none(), "a deopt");

    // A real deopt moves the stamp.
    crate::jit::helpers::despeculate_trapped_method(
        &shared,
        CLASS,
        METHOD,
        DESC,
        cid,
        DeoptReason::BoundsCheck,
        5,
    );
    let moved = osr_reentry_stamp(&shared).expect("memo on");
    assert_ne!(moved, stamp);
    assert!(get(moved, code, 5).is_none());

    // A withdrawn body is never answered, even with fresh stamps.
    osr_reentry_memo_put(&shared, moved, code, cid, &method, &desc, 5, &cm);
    assert!(get(moved, code, 5).is_some());
    cm.retired.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(get(moved, code, 5).is_none(), "retired");
    cm.retired.store(false, std::sync::atomic::Ordering::SeqCst);

    // An OSR denial is asked on every hit.
    shared
        .jit
        .tiered_manager
        .mark_osr_denied(crate::jit::tiered::MethodKey::with_class_id(
            cid,
            Arc::clone(&class),
            Arc::clone(&method),
            Arc::clone(&desc),
        ));
    assert!(get(moved, code, 5).is_none(), "denied");
    shared.jit.tiered_manager.clear_osr_denials();
    assert!(get(moved, code, 5).is_some());

    // The memo holds no strong reference.
    drop(cm);
    assert!(get(moved, code, 5).is_none(), "dropped");

    // A body the door would not reuse empties the memo.
    let mut not_osr = body(0, Vec::new());
    not_osr.compiled_via_osr = false;
    let not_osr = Arc::new(not_osr);
    osr_reentry_memo_put(&shared, moved, code, cid, &method, &desc, 5, &not_osr);
    assert!(get(moved, code, 5).is_none());
}
