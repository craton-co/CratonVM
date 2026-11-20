use super::{
    arm_superseded_optimizing_osr_exits, osr_optimizing_cache_disabled,
    osr_optimizing_refusal_key, osr_optimizing_remembered_other_artifacts,
    queue_superseded_optimizing_osr_bodies, remember_osr_optimizing_artifact, OsrBuildStamps,
};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::Arc;

#[test]
fn t1_w8_l1osr_a_body_replaced_at_every_header_is_queued_once() {
    if osr_optimizing_cache_disabled() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let class_id = ClassId::new(0x9a41);
    let names = ("p/Loop", "main", "([Ljava/lang/String;)V");
    let code: Arc<[u8]> = Arc::from(&[0xb1u8][..]);
    let pins: Arc<[cratonvm_jit::RetainedCode]> = Vec::new().into();
    let body = || {
        let buf = cratonvm_jit::ExecutableBuffer::new(64)
            .expect("executable buffer alloc must succeed in tests");
        Arc::new(cratonvm_jit::CompiledMethod::new(buf))
    };
    let key = |pc| osr_optimizing_refusal_key(shared.vm_identity, class_id, names.1, names.2, pc);
    let stamps = OsrBuildStamps::read(&shared, class_id);
    let old = body();
    let new = body();
    remember_osr_optimizing_artifact(&shared, key(4), &old, &pins, stamps, &code, true);
    remember_osr_optimizing_artifact(&shared, key(9), &old, &pins, stamps, &code, true);
    // Another method of the class is never listed.
    let other = osr_optimizing_refusal_key(shared.vm_identity, class_id, "other", "()V", 4);
    remember_osr_optimizing_artifact(&shared, other, &body(), &pins, stamps, &code, true);
    let rows = || {
        shared
            .jit
            .bridge
            .late_bind_osr_superseded
            .lock()
            .map(|r| r.iter().filter(|row| row.names_body(&old)).count())
            .unwrap_or(0)
    };
    // Replaced under one header only: still served at pc 9, not queued.
    let before = osr_optimizing_remembered_other_artifacts(&shared, class_id, names, Some(&new));
    assert_eq!(before.len(), 1, "one distinct old artifact, not one per header");
    remember_osr_optimizing_artifact(&shared, key(4), &new, &pins, stamps, &code, true);
    queue_superseded_optimizing_osr_bodies(&shared, class_id, names, before);
    assert_eq!(rows(), 0, "still remembered under another header");
    // Replaced under both: queued, and only once.
    let before = osr_optimizing_remembered_other_artifacts(&shared, class_id, names, Some(&new));
    remember_osr_optimizing_artifact(&shared, key(9), &new, &pins, stamps, &code, true);
    queue_superseded_optimizing_osr_bodies(&shared, class_id, names, before);
    assert_eq!(rows(), 1, "queued once no header serves it");
    let again = vec![cratonvm_jit::RetainedCode::new(Arc::clone(&old))];
    queue_superseded_optimizing_osr_bodies(&shared, class_id, names, again);
    assert_eq!(rows(), 1, "never queued twice");
    let queued_names = shared
        .jit
        .bridge
        .late_bind_osr_superseded
        .lock()
        .map(|r| {
            r.iter()
                .any(|row| row.method_name == names.1 && row.descriptor == names.2)
        })
        .unwrap_or(false);
    assert!(queued_names, "the row names the method the arm matches on");
}

/// t1 wave 9, lane JITB (`t1-L1OSR-superseded-osr-rows-hold-bodies-...`):
/// a superseded row holds its body weakly. While a frame's handle keeps
/// the body alive the row survives an arming that cannot arm (no covered
/// header); once the last holder is gone the same arming drops the row,
/// and the row itself never kept the body alive.
#[test]
fn t1_w9_jitb_a_superseded_row_does_not_keep_its_body_alive() {
    let shared = SharedVm::new(VmConfig::default());
    let class_id = ClassId::new(0x9a42);
    let names = ("p/Loop", "main", "([Ljava/lang/String;)V");
    // `return` only: no loop header, so the arm takes its NOT-armed
    // branch and re-pushes the rows that are still live.
    let code: Arc<[u8]> = Arc::from(&[0xb1u8][..]);
    let buf = cratonvm_jit::ExecutableBuffer::new(64)
        .expect("executable buffer alloc must succeed in tests");
    let running = Arc::new(cratonvm_jit::CompiledMethod::new(buf));
    let weak = Arc::downgrade(&running);
    // Pushed directly, as the late-bind forget does (the queue path's
    // `RetainedCode` may park a handle in the retirement queue, which
    // would blur the strong count this test reads).
    if let Ok(mut list) = shared.jit.bridge.late_bind_osr_superseded.lock() {
        let row = super::SupersededOsrBody::new(class_id, names, &running);
        assert!(super::push_superseded_osr_row(&mut list, row));
        let dup = super::SupersededOsrBody::new(class_id, names, &running);
        assert!(!super::push_superseded_osr_row(&mut list, dup), "one row per body");
    }
    let rows = || {
        shared
            .jit
            .bridge
            .late_bind_osr_superseded
            .lock()
            .map(|r| r.len())
            .unwrap_or(0)
    };
    assert_eq!(rows(), 1, "queued while a frame holds the body");
    assert_eq!(Arc::strong_count(&running), 1, "the row holds no strong handle");
    arm_superseded_optimizing_osr_exits(&shared, class_id, names, &code);
    assert_eq!(rows(), 1, "a live row stays for the covering rebuild");
    // The frame leaves the body: its handle was the last one.
    drop(running);
    assert!(weak.upgrade().is_none(), "the row did not keep the body alive");
    arm_superseded_optimizing_osr_exits(&shared, class_id, names, &code);
    assert_eq!(rows(), 0, "a row whose body no frame runs is dropped");
    // A dead body is never queued.
    let buf = cratonvm_jit::ExecutableBuffer::new(64)
        .expect("executable buffer alloc must succeed in tests");
    let gone = Arc::new(cratonvm_jit::CompiledMethod::new(buf));
    let mut list = Vec::new();
    let row = super::SupersededOsrBody::new(class_id, names, &gone);
    drop(gone);
    assert!(!super::push_superseded_osr_row(&mut list, row));
    assert!(list.is_empty());
}
