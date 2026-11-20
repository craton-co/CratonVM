use super::{
    osr_optimizing_cache_disabled, osr_optimizing_installed_dependencies_current,
    osr_optimizing_refusal_key, remember_osr_optimizing_artifact,
    settle_osr_optimizing_install, OsrBuildStamps,
};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::Arc;

#[test]
fn t1_w9_jitb_a_gain_before_install_refuses_and_after_install_spares() {
    if osr_optimizing_cache_disabled() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let class_id = ClassId::new(0x9a43);
    let names = ("p/Loop", "main", "([Ljava/lang/String;)V");
    let code: Arc<[u8]> = Arc::from(&[0xb1u8][..]);
    let pins: Arc<[cratonvm_jit::RetainedCode]> = Vec::new().into();
    let body = || {
        let buf = cratonvm_jit::ExecutableBuffer::new(64)
            .expect("executable buffer alloc must succeed in tests");
        let mut cm = cratonvm_jit::CompiledMethod::new(buf);
        cm.invalidation_epoch = cratonvm_jit::jit_invalidation_epoch();
        Arc::new(cm)
    };
    let key = |pc| osr_optimizing_refusal_key(shared.vm_identity, class_id, names.1, names.2, pc);
    let stamps = OsrBuildStamps::read(&shared, class_id);
    let remembered = |k: (u32, u64, usize)| {
        shared
            .jit
            .bridge
            .osr_optimizing_accepted
            .lock()
            .map(|m| m.contains_key(&k))
            .unwrap_or(false)
    };
    // In flight: the gain lands after the build's check, before install.
    let early = body();
    remember_osr_optimizing_artifact(&shared, key(4), &early, &pins, stamps, &code, true);
    remember_osr_optimizing_artifact(&shared, key(9), &early, &pins, stamps, &code, true);
    if !shared.jit.jit_cache.refuse_in_flight_for_object_observer() {
        return; // `CRATONVM_JIT_OBSERVER_GAIN_REFUSES_IN_FLIGHT=0`: no record.
    }
    assert!(!settle_osr_optimizing_install(&shared, class_id, names, &early));
    assert!(!remembered(key(4)) && !remembered(key(9)), "every entry of the body forgotten");
    // Installed: nothing logged since its compile began.
    let late = body();
    remember_osr_optimizing_artifact(&shared, key(4), &late, &pins, stamps, &code, true);
    assert!(settle_osr_optimizing_install(&shared, class_id, names, &late));
    assert!(remembered(key(4)));
    let through = shared
        .jit
        .bridge
        .osr_optimizing_accepted
        .lock()
        .ok()
        .and_then(|m| {
            m.get(&key(4)).map(|e| {
                e.dependencies_checked_through
                    .load(std::sync::atomic::Ordering::Acquire)
            })
        })
        .unwrap_or(0);
    assert!(through >= late.invalidation_epoch, "re-checks start at the install");
    assert!(shared.jit.jit_cache.refuse_in_flight_for_object_observer());
    assert!(
        osr_optimizing_installed_dependencies_current(&shared, class_id, names, &late, through),
        "a gain after the install spares the installed body"
    );
}
