use super::{
    note_osr_refused_activation_at, osr_loop_key, osr_loop_offer_allowed,
    osr_loop_offer_allowed_at, OSR_REFUSED_ACTIVATIONS_BUDGET, OSR_SPENT_LOOP_REPROBE_INTERVAL,
};
use crate::config::VmConfig;
use crate::runtime::frame::{Frame, OSR_MAX_ATTEMPTS};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::atomic::Ordering;

fn loop_frame(class_id: u32, method: &str) -> Frame {
    Frame::new(
        ClassId::new(class_id),
        "cratonvm/test/OsrBudgetHolder".to_string(),
        method.to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        1,
        1,
        &[],
    )
}

/// A loop header is offered until `OSR_REFUSED_ACTIVATIONS_BUDGET`
/// activations were refused outright, then turned away -- bar one re-probe
/// activation per interval, which starts one refusal short of the budget --
/// and forgotten when the install epoch moves. Other headers are untouched.
#[test]
fn a_loop_refused_in_whole_activations_stops_being_offered_until_the_epoch_moves() {
    let shared = SharedVm::new(VmConfig::default());
    let bridge = &shared.jit.bridge;
    let (key, other) = ((7, 0xfeed, 12), (7, 0xfeed, 40));
    let epoch = 5;
    for n in 1..OSR_REFUSED_ACTIVATIONS_BUDGET {
        assert!(osr_loop_offer_allowed_at(bridge, key, epoch));
        assert_eq!(note_osr_refused_activation_at(bridge, key, epoch), n);
    }
    assert!(osr_loop_offer_allowed_at(bridge, key, epoch));
    assert_eq!(
        note_osr_refused_activation_at(bridge, key, epoch),
        OSR_REFUSED_ACTIVATIONS_BUDGET
    );
    for _ in 1..OSR_SPENT_LOOP_REPROBE_INTERVAL {
        assert!(!osr_loop_offer_allowed_at(bridge, key, epoch));
    }
    assert!(
        osr_loop_offer_allowed_at(bridge, key, epoch),
        "the re-probe"
    );
    assert!(
        osr_loop_offer_allowed_at(bridge, key, epoch),
        "a re-probe that was not refused leaves the loop offered"
    );
    assert_eq!(
        note_osr_refused_activation_at(bridge, key, epoch),
        OSR_REFUSED_ACTIVATIONS_BUDGET,
        "one more refused activation re-spends it"
    );
    assert!(!osr_loop_offer_allowed_at(bridge, key, epoch));
    assert!(osr_loop_offer_allowed_at(bridge, other, epoch));
    assert_eq!(bridge.osr_refused_loop_count.load(Ordering::Acquire), 1);
    // A redefinition or flush moves the epoch: the record is dropped.
    assert!(osr_loop_offer_allowed_at(bridge, key, epoch + 1));
    assert_eq!(bridge.osr_refused_loop_count.load(Ordering::Acquire), 0);
    assert_eq!(note_osr_refused_activation_at(bridge, key, epoch + 1), 1);
}

/// The record is per VM (each VM's `JitBridgeState` holds its own map)
/// and per loop header of one method (the key; since wave 20 it carries
/// no VM identity, which the per-VM map makes redundant).
#[test]
fn the_loop_budget_key_is_per_vm_method_and_pc() {
    let a = SharedVm::new(VmConfig::default());
    let b = SharedVm::new(VmConfig::default());
    let spin = loop_frame(3, "spin");
    let key = osr_loop_key(&spin, 9);
    assert_ne!(key, osr_loop_key(&spin, 10));
    assert_ne!(key, osr_loop_key(&loop_frame(3, "spin2"), 9));
    assert_ne!(key, osr_loop_key(&loop_frame(4, "spin"), 9));
    for _ in 0..OSR_REFUSED_ACTIVATIONS_BUDGET {
        note_osr_refused_activation_at(&a.jit.bridge, key, a.jit.code_state_epoch());
    }
    // Another VM has no record, and asks nothing past one load.
    assert_eq!(
        b.jit.bridge.osr_refused_loop_count.load(Ordering::Acquire),
        0
    );
    assert!(osr_loop_offer_allowed(&b, &spin, 9));
}

/// The interpreter charges the method-wide budget once per activation,
/// on the refusal that exhausts the frame's own five offers -- or at once
/// for a verdict that retires the loop header for the activation.
#[test]
fn a_refused_activation_is_charged_once_on_the_refusal_that_exhausts_it() {
    let shared = SharedVm::new(VmConfig::default());
    let count = || {
        shared
            .jit
            .bridge
            .osr_refused_loop_count
            .load(Ordering::Acquire)
    };
    let mut thread = JvmThread::new(ThreadId(0), "osr-budget");
    thread.frames.push(loop_frame(5, "spin"));
    for n in 1..OSR_MAX_ATTEMPTS {
        crate::runtime::interpreter::charge_osr_rejection(&shared, &mut thread, 0, 4, 1);
        assert_eq!(thread.frames[0].osr_attempts_at(4), Some(n));
        assert_eq!(count(), 0, "not exhausted yet");
    }
    crate::runtime::interpreter::charge_osr_rejection(&shared, &mut thread, 0, 4, 1);
    assert_eq!(thread.frames[0].osr_attempts_at(4), Some(OSR_MAX_ATTEMPTS));
    assert_eq!(count(), 1);
    let key = osr_loop_key(&thread.frames[0], 4);
    let refused = |shared: &SharedVm| {
        shared
            .jit
            .bridge
            .osr_refused_loops
            .lock()
            .ok()
            .and_then(|m| m.get(&key).map(|r| r.refused_activations))
    };
    assert_eq!(refused(&shared), Some(1));
    // Already exhausted: nothing further is charged.
    crate::runtime::interpreter::charge_osr_rejection(&shared, &mut thread, 0, 4, 1);
    assert_eq!(refused(&shared), Some(1));
    // A retiring verdict at another header charges it at once.
    crate::runtime::interpreter::charge_osr_rejection(
        &shared,
        &mut thread,
        0,
        8,
        OSR_MAX_ATTEMPTS,
    );
    assert_eq!(thread.frames[0].osr_attempts_at(8), Some(OSR_MAX_ATTEMPTS));
    assert_eq!(count(), 2);
}

/// The TDigest `(int) -> double` lambda peephole enters a compiled `get`
/// directly; in interpreter-only mode it takes the generic `apply` instead,
/// before any compiled entry or compile. A text pin: the helper cannot be
/// driven without compiled code.
#[test]
fn the_lambda_int_to_double_peephole_stands_down_in_interpreter_only_mode() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let start = src
        .find("\npub unsafe fn jit_lambda_int_to_double_body(")
        .expect("the peephole body exists");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)];
    // Round 12 wave 1 moved the compile arm into `resolve_lambda_get_route`,
    // which the body calls after the gate: read the two in that order.
    let route_at = src
        .find("\npub unsafe fn resolve_lambda_get_route(")
        .expect("the route resolver exists");
    let route_rest = &src[route_at + 1..];
    let route = &route_rest[..route_rest
        .find("\n\x7d\n")
        .map_or(route_rest.len(), |e| e + 3)];
    assert!(
        body.contains("resolve_lambda_get_route("),
        "the body resolves its route"
    );
    let body = format!("{body}{route}");
    let body = body.as_str();
    let gate = body
        .find("if crate::runtime::interpreter::jvmti_requires_interpreter(vm) \x7b")
        .expect("the peephole asks the interpreter-only question");
    // Round 12 wave 2 (lane osrfix): the compiled `get` is entered under
    // `lambda_get_call_then_route`, which services its sentinel.
    for later in [
        "try_jit_compile_callee_for_class(",
        "lambda_get_call_then_route(",
        "try_call_compiled_entry_reentrant(",
    ] {
        let at = body.find(later).expect("the peephole still has this arm");
        assert!(gate < at, "the gate precedes {later}");
    }
}
