use std::sync::atomic::Ordering;

#[test]
fn only_this_vms_definitions_retire_the_absent_answer() {
    let mine = crate::vm::SharedVm::new(crate::VmConfig::default());
    let other = crate::vm::SharedVm::new(crate::VmConfig::default());
    let slot = |vm: &crate::vm::SharedVm| {
        cratonvm_classloading::store_epoch_slot(vm.classes.class_layout_domain)
    };
    if slot(&mine) == slot(&other) {
        // The two stores share an epoch slot: nothing to assert.
        return;
    }
    assert_eq!(
        mine.classes.class_layout_domain, mine.jit.class_layout_domain,
        "one token per VM"
    );
    let define = |vm: &crate::vm::SharedVm, name: &str| {
        vm.classes
            .class_manager
            .write()
            .try_ensure_synthetic_class(name, 0)
            .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
    };
    let epoch = || {
        cratonvm_classloading::store_epochs(mine.classes.class_layout_domain)
            .class_definition_epoch()
    };
    let held = || {
        mine.classes
            .annotation_proxy_absent_epoch
            .load(Ordering::Relaxed)
            == epoch()
    };
    let mut checked = false;
    for round in 0..50 {
        let before = epoch();
        if mine.classes.resolve_annotation_proxy_cid().is_some() {
            // The proxy class exists in this VM: no absence to hold.
            return;
        }
        define(
            &other,
            format!("cratonvm/test/W21L5AnnOther{round}").as_str(),
        );
        if epoch() != before {
            // A concurrent test's store shares this VM's slot.
            continue;
        }
        assert!(
            held(),
            "another VM's class definition must not retire this VM's answer"
        );
        checked = true;
        break;
    }
    assert!(checked, "no quiet window for this VM's epoch slot");
    define(&mine, "cratonvm/test/W21L5AnnMine");
    assert!(!held(), "this VM's own definition retires it");
}
