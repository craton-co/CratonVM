use super::seed_profile_replay_if_definitions_moved;
use std::sync::Arc;

fn define(vm: &crate::vm::SharedVm, name: &str) {
    vm.classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(name, 0)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
}

/// A VM's first visit walks; a repeat visit, or one after ANOTHER VM
/// defined a class, does not; one after this VM defined a class does.
#[test]
fn only_this_vms_definitions_reopen_its_profile_replay_sweep() {
    let mine = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let other = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let slot = |vm: &crate::vm::SharedVm| {
        cratonvm_classloading::store_epoch_slot(vm.jit.class_layout_domain)
    };
    if slot(&*mine) == slot(&*other) {
        // The two stores share an epoch slot: nothing to assert.
        return;
    }
    let epoch = || {
        cratonvm_classloading::store_epochs(mine.jit.class_layout_domain)
            .class_definition_epoch()
    };
    assert!(
        seed_profile_replay_if_definitions_moved(&mine),
        "a VM never swept walks"
    );
    let mut checked = false;
    for round in 0..50 {
        // Read BEFORE the latching visit, so an unchanged epoch at the
        // end proves the latch holds it.
        let before = epoch();
        let _ = seed_profile_replay_if_definitions_moved(&mine);
        define(&other, &format!("cratonvm/test/W21L5ReplayOther{round}"));
        let walked = seed_profile_replay_if_definitions_moved(&mine);
        if epoch() != before {
            // A concurrent test's store shares this VM's slot.
            continue;
        }
        assert!(
            !walked,
            "another VM's class definition must not re-open this VM's sweep"
        );
        checked = true;
        break;
    }
    assert!(checked, "no quiet window for this VM's epoch slot");
    define(&mine, "cratonvm/test/W21L5ReplayMine");
    assert!(
        seed_profile_replay_if_definitions_moved(&mine),
        "this VM's own definition re-opens it"
    );
}
