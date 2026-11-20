use super::*;
use crate::config::VmConfig;

pub fn define(vm: &SharedVm, name: &str) {
    vm.classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(name, 0)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
}

/// Two VMs whose class stores do not share an epoch slot, or `None` when
/// the domains collide (the documented conservative case).
pub fn two_vms() -> Option<(SharedVm, SharedVm)> {
    let mine = SharedVm::new(VmConfig::default());
    let other = SharedVm::new(VmConfig::default());
    (cratonvm_classloading::store_epoch_slot(mine.jit.class_layout_domain)
        != cratonvm_classloading::store_epoch_slot(other.jit.class_layout_domain))
    .then_some((mine, other))
}

/// A typecheck answer proven in one VM survives a class definition in
/// another VM and is retired by one in its own.
#[test]
pub fn a_typecheck_answer_is_retired_only_by_its_own_vms_definitions() {
    let Some((mine, other)) = two_vms() else {
        return;
    };
    let key = (mine.vm_identity, 0x21_0050, 5, 77, false);
    let mut checked = false;
    for round in 0..50 {
        let epoch = typecheck_memo_epoch(&mine);
        jit_typecheck_answer_cache_put(key, epoch);
        define(&other, &format!("cratonvm/test/W21L5TcOther{round}"));
        let now = typecheck_memo_epoch(&mine);
        if now != epoch {
            // A concurrent test's store shares this VM's slot.
            continue;
        }
        assert!(
            jit_typecheck_answer_cached(key, now),
            "another VM's class definition must not retire this VM's answer"
        );
        checked = true;
        break;
    }
    assert!(checked, "no quiet window for this VM's epoch slot");
    let before = typecheck_memo_epoch(&mine);
    define(&mine, "cratonvm/test/W21L5TcMine");
    let after = typecheck_memo_epoch(&mine);
    assert_ne!(after, before, "a definition in this VM moves its epoch");
    assert!(!jit_typecheck_answer_cached(key, after));
}

/// The stamp is tagged with the store's slot, so it never equals a
/// process-wide snapshot or another slot's stamp.
#[test]
pub fn a_typecheck_stamp_names_its_slot() {
    let Some((mine, other)) = two_vms() else {
        return;
    };
    assert_ne!(typecheck_memo_epoch(&mine), typecheck_memo_epoch(&other));
    assert_ne!(
        typecheck_memo_epoch(&mine),
        crate::classloading::class_definition_epoch()
    );
}

/// The class-identity flush empties this thread's name-keyed dispatch
/// memos when ITS VM defines a class, not when another VM does.
#[test]
pub fn the_class_identity_flush_follows_its_own_vms_definitions() {
    let Some((mine, other)) = two_vms() else {
        return;
    };
    let key = jit_site_key(mine.vm_identity, 0x21_0051);
    let present = || WRAPPED_STATIC_CALLEE.with(|c| c.borrow().contains_key(&key));
    let quiet = || {
        (
            cratonvm_classloading::store_epochs(mine.jit.class_layout_domain)
                .class_definition_epoch(),
            crate::classloading::any_class_redefined(),
        )
    };
    let mut checked = false;
    for round in 0..50 {
        let before = quiet();
        flush_class_identity_dispatch_memos(&mine);
        WRAPPED_STATIC_CALLEE.with(|c| {
            c.borrow_mut().insert(key, ());
        });
        define(&other, &format!("cratonvm/test/W21L5FlOther{round}"));
        flush_class_identity_dispatch_memos(&mine);
        if quiet() != before {
            continue;
        }
        assert!(
            present(),
            "another VM's class definition must not flush this VM's memos"
        );
        checked = true;
        break;
    }
    assert!(checked, "no quiet window for this VM's epoch slot");
    define(&mine, "cratonvm/test/W21L5FlMine");
    flush_class_identity_dispatch_memos(&mine);
    assert!(!present(), "this VM's own definition flushes");
}
