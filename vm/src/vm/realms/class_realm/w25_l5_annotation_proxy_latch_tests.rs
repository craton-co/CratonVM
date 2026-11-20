#[test]
fn another_vms_annotation_proxy_does_not_raise_this_vms_latch() {
    let mine = crate::vm::SharedVm::new(crate::VmConfig::default());
    let other = crate::vm::SharedVm::new(crate::VmConfig::default());
    let slot = |vm: &crate::vm::SharedVm| {
        cratonvm_classloading::store_epoch_slot(vm.classes.class_layout_domain)
    };
    let latch = |vm: &crate::vm::SharedVm| {
        cratonvm_classloading::store_epochs(vm.classes.class_layout_domain)
            .annotation_proxy_defined()
    };
    if slot(&mine) == slot(&other) || latch(&mine) {
        // The two stores share a slot, or a store sharing this VM's slot
        // (a concurrent test's VM) minted the class already: nothing to
        // assert.
        return;
    }
    other
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(super::ANNOTATION_PROXY_CLASS, 4)
        .expect("a VM-internal carrier is admitted in every mode");
    assert!(latch(&other), "the defining store's latch is raised");
    assert!(
        !latch(&mine),
        "another VM's annotation proxy must not raise this VM's latch"
    );
}
