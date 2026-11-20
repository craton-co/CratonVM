use super::door_precise_resume_allowed;
use crate::classloading::resolution::CachedBytecodeMethod;
use std::sync::Arc;

fn method(is_synchronized: bool, code: &[u8]) -> Arc<CachedBytecodeMethod> {
    Arc::new(CachedBytecodeMethod::from_parts(
        cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: cratonvm_types::ClassId::new(0),
            class_name: Arc::from("C"),
            method_name: Arc::from("m"),
            method_descriptor: Arc::from("()V"),
            source_file: None,
            code: Arc::from(code),
            exception_table: Arc::from(Vec::new().into_boxed_slice()),
            max_stack: 4,
            max_locals: 2,
            num_params: 0,
            is_synchronized,
            is_static: true,
        },
    ))
}

/// The doors hand the method monitor to a resumed frame, so the additive
/// arm does not refuse an `ACC_SYNCHRONIZED` body there; a body that
/// enters a monitor itself is still refused.
#[test]
fn the_door_gate_resumes_a_synchronized_body_without_monitor_bytecode() {
    if !cratonvm_jit::deopt_sink_resume_enabled() {
        return;
    }
    // iconst_1 putstatic #1 iconst_0 istore_0 return
    let plain = [0x04u8, 0xb3, 0x00, 0x01, 0x03, 0x3b, 0xb1];
    // aload_0 monitorenter aload_0 monitorexit return
    let locking = [0x2au8, 0xc2, 0x2a, 0xc3, 0xb1];
    let frame = cratonvm_jit::deopt::ReconstructedFrame {
        class_id: 0,
        method_key: "C.m:()V".to_string(),
        bci: 4,
        semantics: cratonvm_jit::deopt::ResumeSemantics::REEXECUTE,
        ..Default::default()
    };
    assert!(door_precise_resume_allowed(&method(true, &plain), &frame));
    assert!(door_precise_resume_allowed(&method(false, &plain), &frame));
    let mut early = frame.clone();
    early.bci = 1;
    assert!(!door_precise_resume_allowed(
        &method(true, &locking),
        &early
    ));
}

// The door re-run truth table is `deopt_resume::unsound_own_stash_rerun_refused`'s
// since round 13 wave 5 (lane resume); `r13w3_replay2_rerun_answer_tests` pins it.
