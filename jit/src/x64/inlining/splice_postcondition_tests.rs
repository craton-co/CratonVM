use super::splice_point_is_misidentified;
use crate::deopt::{
    DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, FrameValue, ResumeSemantics,
};

const CALLER: &str = "Caller.run:()V";
const CALLEE: &str = "Callee.leaf:(I)I";
const NESTED: &str = "Nested.deeper:()V";

/// The spliced callee's `code_len`, as `InlineSite::callee_code_len` reports
/// it. Every `frame` below sits at bci 3, which is inside it, so the
/// bci-range clause is satisfied unless a test says otherwise — the
/// identity-shaped assertions are then about identity only.
const CALLEE_CODE_LEN: usize = 8;

fn frame(method_key: &str, caller: Option<Box<FrameState>>) -> FrameState {
    FrameState {
        class_id: 0,
        method_key: method_key.to_string(),
        bci: 3,
        locals: vec![FrameValue::Int(1)],
        stack: Vec::new(),
        monitors: Vec::new(),
        caller,
    }
}

fn point(frame_state: FrameState) -> DeoptimizationPoint {
    DeoptimizationPoint {
        native_offset: 0x20,
        bci: frame_state.bci,
        reason: DeoptReason::BoundsCheck,
        action: DeoptAction::Reinterpret,
        speculation_id: 0,
        semantics: ResumeSemantics::REEXECUTE,
        frame_state,
    }
}

/// A point with no caller scope does not say it came from inside a splice.
/// This is the case the old check caught, and it still refuses.
#[test]
fn a_point_with_no_caller_scope_is_refused() {
    assert!(splice_point_is_misidentified(
        &point(frame(CALLEE, None)),
        CALLEE,
        CALLER,
        CALLEE_CODE_LEN
    ));
}

/// The case the old check MISSED, and the one the single-pass backend
/// actually produces: a caller chain is present, so the old rule admitted
/// it, while the innermost frame still names the enclosing method at a bci
/// that belongs to the callee.
#[test]
fn a_caller_scoped_point_naming_the_caller_is_still_refused() {
    let p = point(frame(CALLER, Some(Box::new(frame(CALLER, None)))));
    assert!(
        splice_point_is_misidentified(&p, CALLEE, CALLER, CALLEE_CODE_LEN),
        "a caller chain says there is an outer frame; it says nothing about \
         whether the innermost one describes the callee"
    );
}

/// What the check is meant to admit, once `build_frame_state_at` can stamp
/// the callee's identity: an innermost frame naming the callee, under a
/// caller scope naming the enclosing method.
#[test]
fn a_callee_identified_point_under_a_caller_scope_is_admitted() {
    let p = point(frame(CALLEE, Some(Box::new(frame(CALLER, None)))));
    assert!(!splice_point_is_misidentified(
        &p,
        CALLEE,
        CALLER,
        CALLEE_CODE_LEN
    ));
}

/// A point a NESTED splice published names the inner callee, and the outer
/// splice's own postcondition must not refuse it for that.
///
/// The outer check scans every point the outer body added, which includes
/// the inner body's. Requiring all of them to name the OUTER callee would
/// make nesting and publishing mutually exclusive, and would buy nothing:
/// the inner splice held that point to this same rule with its own callee
/// key a moment earlier.
#[test]
fn a_nested_splices_point_is_admitted_by_the_enclosing_splice() {
    let p = point(frame(
        NESTED,
        Some(Box::new(frame(CALLEE, Some(Box::new(frame(CALLER, None)))))),
    ));
    assert!(
        !splice_point_is_misidentified(&p, NESTED, CALLER, CALLEE_CODE_LEN),
        "the inner splice admits its own point"
    );
    assert!(
        !splice_point_is_misidentified(&p, CALLEE, CALLER, CALLEE_CODE_LEN),
        "and the outer splice must not refuse it for naming the inner callee"
    );
}

/// An empty key is the legacy-producer / superseded-guard-sentinel shape,
/// which `deopt_frame_matches_method` refuses outright. A splice must not
/// publish one either: it identifies nothing, so it cannot be checked
/// against anything.
#[test]
fn a_point_with_an_empty_method_key_is_refused() {
    let p = point(frame("", Some(Box::new(frame(CALLER, None)))));
    assert!(splice_point_is_misidentified(
        &p,
        CALLEE,
        CALLER,
        CALLEE_CODE_LEN
    ));
}

/// The CONVERSE malformed pair, and the one the emitter would actually
/// produce first (round 10 wave 7): the callee's identity on an
/// ENCLOSING-method bci.
///
/// `emit_post_invoke_exception_check` and friends key their reason-9 frame on
/// `Compiler::dbg_last_pc`, which only the OUTER walk assigns, so inside a
/// splice the pc is the caller's invoke while `current_bytecode_owner` stamps
/// the callee. bci 40 is past the end of an 8-byte callee, which is what that
/// looks like whenever the enclosing method is the larger of the two — the
/// usual case, since the callee was inlined into it.
#[test]
fn a_callee_identified_point_at_an_enclosing_bci_is_refused() {
    let mut fs = frame(CALLEE, Some(Box::new(frame(CALLER, None))));
    fs.bci = 40;
    let mut p = point(fs);
    // `build_and_record_deopt_point` publishes one `resume_bci_for` result
    // into both fields; keep the fixture's two in step so this test is about
    // the range clause and not about a shape no producer emits.
    p.bci = 40;
    assert!(
        splice_point_is_misidentified(&p, CALLEE, CALLER, CALLEE_CODE_LEN),
        "a bci outside the callee's own code cannot be a callee pc, whatever \
         the key says"
    );
    // The control: the same point inside the callee's code is admitted, so
    // the clause is a range test and not a blanket refusal of every
    // callee-identified point.
    let mut inside = point(frame(CALLEE, Some(Box::new(frame(CALLER, None)))));
    inside.bci = inside.frame_state.bci;
    assert!(!splice_point_is_misidentified(
        &inside,
        CALLEE,
        CALLER,
        CALLEE_CODE_LEN
    ));
}

/// The enclosing owner of a NESTED splice is a CALLEE, not the compiling
/// method, and the check is handed that key rather than
/// `Compiler::method_key`.
///
/// Without this the nested case has no guard at all: a point stamped with
/// the ENCLOSING CALLEE's key at an inner-callee bci is the same malformed
/// pair one level down, and comparing it against the root's key admits it.
/// No splice nests through `try_emit_inline_site` today
/// (`try_emit_nested_inline` pushes no scope and refuses any publication),
/// so this pins the shape the check must keep refusing if one ever does.
#[test]
fn the_enclosing_callee_is_refused_for_a_nested_splice() {
    let p = point(frame(
        CALLEE,
        Some(Box::new(frame(CALLEE, Some(Box::new(frame(CALLER, None)))))),
    ));
    assert!(
        splice_point_is_misidentified(&p, NESTED, CALLEE, CALLEE_CODE_LEN),
        "the enclosing owner of a nested splice is the outer CALLEE; naming it \
         is the caller-named/callee-bci'd pair one level down"
    );
    assert!(
        !splice_point_is_misidentified(&p, NESTED, CALLER, CALLEE_CODE_LEN),
        "and comparing against the ROOT method's key — what this check was \
         handed before wave 7 — admits it, which is why the enclosing owner \
         is read from `current_bytecode_owner`"
    );
}
