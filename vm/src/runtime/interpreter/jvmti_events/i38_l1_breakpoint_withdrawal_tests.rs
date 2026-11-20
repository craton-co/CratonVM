use super::*;
use crate::config::VmConfig;
use std::sync::Arc;

/// A one-`RET` body, publishable under any key.
fn body() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    cratonvm_jit::CompiledMethod::new(buf)
}

fn publish(shared: &SharedVm, class: &str, method: &str, id: ClassId) {
    shared.jit.jit_cache.put(
        Arc::from(class),
        Arc::from(method),
        Arc::from("()V"),
        id,
        body(),
    );
}

#[test]
fn a_breakpoint_withdraws_its_class_and_spares_the_others() {
    if !BREAKPOINT_SCOPED_WITHDRAWAL_ENABLED {
        return;
    }
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let bp = ClassId::new(0x3801);
    let other = ClassId::new(0x3802);
    publish(&shared, "i38/l1/Bp", "callee", bp);
    publish(&shared, "i38/l1/Bp", "loop", bp);
    publish(&shared, "i38/l1/Other", "run", other);
    let cache = &shared.jit.jit_cache;
    let callee = cache
        .get("i38/l1/Bp", "callee", "()V", bp)
        .expect("published");
    let before = shared.jit.code_state_epoch();

    note_breakpoint_classes_gained(&shared, &[(bp, Some("i38/l1/Bp".to_string()))]);
    assert!(cache.get("i38/l1/Bp", "callee", "()V", bp).is_none());
    assert!(cache.get("i38/l1/Bp", "loop", "()V", bp).is_none());
    assert!(
        callee.is_withdrawn_by_redefinition(),
        "a frame running it leaves at its next exit-capable point"
    );
    assert!(
        cache.get("i38/l1/Other", "run", "()V", other).is_some(),
        "another class's body stays compiled"
    );
    assert_eq!(
        shared.jit.code_state_epoch(),
        before,
        "a scoped withdrawal expires no queued compile"
    );
    assert!(
        cache.compiled_since_redefinition_of(bp, callee.install_epoch),
        "no redefinition barrier for a class that was not redefined"
    );
    assert!(!shared.debug.debugger_gates.every_body_withdrawn());

    // A JDK class, whose methods the JIT's intrinsics expand without a
    // record, takes the whole-cache path; a program's class does not.
    assert!(jit_may_expand_a_method_of_without_a_record(
        "java/lang/String"
    ));
    assert!(!jit_may_expand_a_method_of_without_a_record("i38/l1/Bp"));

    // A class the caller cannot name: every body goes.
    note_breakpoint_classes_gained(&shared, &[(ClassId::new(0x3803), None)]);
    assert!(cache.get("i38/l1/Other", "run", "()V", other).is_none());
    assert!(!shared.debug.debugger_gates.every_body_withdrawn());
}
