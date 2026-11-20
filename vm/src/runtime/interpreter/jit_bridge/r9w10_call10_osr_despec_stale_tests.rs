use super::cached_osr_body_is_despec_stale;
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_jit::deopt::{DeoptReason, SPECULATION_ID_ANY};
use std::sync::Arc;

const CLASS: &str = "r9w10/TryLoop";
const METHOD: &str = "sumThrow";
const DESC: &str = "([II)J";
const KEY: &str = "r9w10/TryLoop.sumThrow:([II)J";

/// An OSR artifact stamped with `epoch`, carrying one deopt point of
/// `reason` at `bci` (the metadata only; the `ret` body never runs).
fn osr_body(epoch: u64, bci: u32, reason: DeoptReason) -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.compilation_epoch = epoch;
    cm.deopt_points
        .push(cratonvm_jit::deopt::DeoptimizationPoint {
            native_offset: 0,
            bci,
            reason,
            action: cratonvm_jit::deopt::DeoptAction::Reinterpret,
            semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
            speculation_id: cratonvm_jit::deopt::speculation_id(bci, reason),
            frame_state: cratonvm_jit::deopt::FrameState {
                class_id: 0,
                method_key: String::new(),
                bci,
                locals: Vec::new(),
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
        });
    cm
}

fn stale(shared: &SharedVm, cm: &cratonvm_jit::CompiledMethod) -> bool {
    cached_osr_body_is_despec_stale(
        shared,
        cm,
        &Arc::from(CLASS),
        &Arc::from(METHOD),
        &Arc::from(DESC),
    )
}

/// `TryLoop throw`'s shape: an OSR body compiled before its loop-header
/// guard was de-speculated is recompiled, not reused -- and only then.
#[test]
fn a_cached_osr_body_keeping_a_despeculated_guard_is_stale() {
    if !cratonvm_jit::deopt_real_enabled() {
        return; // no epochs are stamped; the predicate answers `false`
    }
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let body = osr_body(0, 5, DeoptReason::BoundsCheck);
    assert!(
        !stale(&shared, &body),
        "nothing de-speculated, epoch current"
    );

    shared.bump_compilation_epoch(KEY);
    assert!(
        !stale(&shared, &body),
        "a deopt alone does not refuse reuse"
    );

    shared
        .jit
        .despec_registry
        .insert(KEY, 5, SPECULATION_ID_ANY);
    assert!(stale(&shared, &body), "the guard at bci 5 was withdrawn");

    // A recompile is stamped with the live epoch: it is reused even if it
    // still carried the guard, until the next charged deopt.
    let fresh = osr_body(
        shared.compilation_epoch_for(KEY),
        5,
        DeoptReason::BoundsCheck,
    );
    assert!(!stale(&shared, &fresh));

    // A de-spec at another bci does not touch this body's guard.
    let other = osr_body(0, 9, DeoptReason::BoundsCheck);
    assert!(!stale(&shared, &other));

    // An OSR-exit map is not a speculation: the recompile keeps it.
    let exit_only = osr_body(0, 5, DeoptReason::OsrExit);
    assert!(!stale(&shared, &exit_only));

    // A not-compilable method keeps its stale body: the recompile would be
    // refused, and no OSR body at all is worse.
    shared.jit.jit_skip_set.write().insert((
        Arc::from(CLASS),
        Arc::from(METHOD),
        Arc::from(DESC),
    ));
    assert!(!stale(&shared, &body));
}
