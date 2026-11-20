use super::mh_by_id_body_admissible;
use std::sync::atomic::Ordering;

fn body() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("alloc executable");
    buf.emit(&[0xC3]); // RET
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.method_declares_no_handlers = true;
    cm
}

#[test]
fn a_plain_live_handlerless_body_is_admitted() {
    assert!(mh_by_id_body_admissible(&body()));
}

#[test]
fn every_by_name_service_and_a_stale_body_refuse() {
    let mut cm = body();
    cm.method_declares_no_handlers = false;
    assert!(!mh_by_id_body_admissible(&cm), "handlers: a re-run resolves by name");
    let cm = body();
    cm.retired.store(true, Ordering::Release);
    assert!(!mh_by_id_body_admissible(&cm));
    let cm = body();
    cm.superseded.store(true, Ordering::Release);
    assert!(!mh_by_id_body_admissible(&cm));
    let mut cm = body();
    cm.can_osr_exit = true;
    assert!(!mh_by_id_body_admissible(&cm));
    let mut cm = body();
    cm.has_indy_trap = true;
    assert!(!mh_by_id_body_admissible(&cm));
    let mut cm = body();
    cm.has_frameless_trap_stub = true;
    assert!(!mh_by_id_body_admissible(&cm));
    let mut cm = body();
    cm.trap_cause_points = vec![(1u64, 8u8)].into_boxed_slice();
    assert!(!mh_by_id_body_admissible(&cm));
    let mut cm = body();
    cm.osr_exit_points = vec![4];
    assert!(!mh_by_id_body_admissible(&cm));
    let mut cm = body();
    cm.post_call_own_exit_bcis = vec![4];
    assert!(!mh_by_id_body_admissible(&cm));
    let mut cm = body();
    cm.requires_wrapped_entry = true;
    assert!(!mh_by_id_body_admissible(&cm));
    let mut cm = body();
    cm.self_locks_monitor = true;
    assert!(!mh_by_id_body_admissible(&cm));
}
