use super::*;
use crate::deopt::{splice_scope_class_hint, SpliceScopeClass, SPLICE_SCOPE_CLASS_AMBIGUOUS};

fn site(base: usize, key: &str, class_id: u32) -> IrInlineSite {
    IrInlineSite {
        base,
        code_len: 4,
        num_args: 1,
        max_locals: 1,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: false,
        method_key: key.to_string(),
        class_id,
    }
}

/// One row per key, sorted, so the VM's binary search finds it; the same
/// body spliced at two sites is one row.
#[test]
fn rows_are_sorted_and_one_per_key() {
    let mut sites = HashMap::new();
    sites.insert(10, site(100, "Q.b:()I", 9));
    sites.insert(20, site(110, "P.a:()I", 7));
    sites.insert(30, site(120, "Q.b:()I", 9));
    let rows = splice_scope_class_ids(&sites);
    assert_eq!(
        rows,
        vec![("P.a:()I".to_string(), 7), ("Q.b:()I".to_string(), 9)]
    );
    assert_eq!(
        splice_scope_class_hint(&rows, "P.a:()I"),
        SpliceScopeClass::Exact(7)
    );
    assert_eq!(
        splice_scope_class_hint(&rows, "Q.b:()I"),
        SpliceScopeClass::Exact(9)
    );
    assert_eq!(
        splice_scope_class_hint(&rows, "R.c:()I"),
        SpliceScopeClass::Unknown
    );
    assert_eq!(
        splice_scope_class_hint(&[], "P.a:()I"),
        SpliceScopeClass::Unknown
    );
}

/// Two loaders' same-named classes in one compile: the key is marked
/// ambiguous, never answered with either id.
#[test]
fn a_key_two_classes_share_is_ambiguous() {
    let mut sites = HashMap::new();
    sites.insert(10, site(100, "P.a:()I", 7));
    sites.insert(20, site(110, "P.a:()I", 8));
    let rows = splice_scope_class_ids(&sites);
    assert_eq!(
        rows,
        vec![("P.a:()I".to_string(), SPLICE_SCOPE_CLASS_AMBIGUOUS)]
    );
    assert_eq!(
        splice_scope_class_hint(&rows, "P.a:()I"),
        SpliceScopeClass::Ambiguous
    );
}

/// A site the resolver gave no class id adds nothing: its scope keeps the
/// name rule, and it does not make a known id ambiguous.
#[test]
fn a_site_without_a_class_id_adds_no_row() {
    let mut sites = HashMap::new();
    sites.insert(10, site(100, "P.a:()I", 0));
    assert!(splice_scope_class_ids(&sites).is_empty());
    sites.insert(20, site(110, "P.a:()I", 7));
    let rows = splice_scope_class_ids(&sites);
    assert_eq!(
        splice_scope_class_hint(&rows, "P.a:()I"),
        SpliceScopeClass::Exact(7)
    );
}

#[test]
fn reconstruct_frame_preserves_per_scope_class_id() {
    use crate::deopt::{
        reconstruct_frame_from_machine_state, DeoptAction, DeoptReason, DeoptimizationPoint,
        FrameState, ResumeSemantics, SavedRegisters,
    };
    let caller = FrameState {
        class_id: 42,
        method_key: "A.caller:()V".to_string(),
        bci: 4,
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: Vec::new(),
        caller: None,
    };
    let point = DeoptimizationPoint {
        native_offset: 0,
        bci: 7,
        reason: DeoptReason::NullCheck,
        action: DeoptAction::RecompileAndReinterpret,
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 99,
            method_key: "B.callee:()V".to_string(),
            bci: 7,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: Some(Box::new(caller)),
        },
        semantics: ResumeSemantics::REEXECUTE,
    };
    let rf = reconstruct_frame_from_machine_state(&point, &SavedRegisters::default(), 0);
    assert_eq!(rf.class_id, 99);
    assert_eq!(rf.caller_frames.len(), 1);
    assert_eq!(rf.caller_frames[0].class_id, 42);
}
