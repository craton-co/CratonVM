use super::*;
use crate::config::VmConfig;
use std::sync::Arc;

fn vm_with_a_mirror_slot(name: &str) -> (Arc<SharedVm>, ClassId, usize) {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let cid = {
        let mut cm = shared.classes.class_manager.write();
        let Ok(cid) = cm.try_ensure_synthetic_class(name, 0) else {
            panic!("Compatible mode fabricates this fixture");
        };
        cid        };
    let mirror = shared.mem.heap.alloc_object(cid, 0);
    shared.classes.class_mirrors.write().insert(cid, mirror);
    let slot = class_mirror_slot_for(&shared, cid);
    assert_ne!(slot, 0);
    (shared, cid, slot)
}

/// `r13w10-sync6-ldc-global-slots-outlive-their-vm`, and MISC11-1 (round
/// 14 wave 2): the slot table is per VM, so a VM's rows die with it and
/// one VM's rows, even under the same `(class id, cp index)` key, are
/// neither seen nor retired by another.
#[test]
fn each_vm_has_its_own_ldc_slot_table() {
    if ldc_slot_memo_latched_off() || !compiled_ldc_const_cache_enabled() {
        // No slot is ever minted under the kill switches.
        return;
    }
    let name = "cratonvm/test/R13Misc11LdcPerVm";
    let (a, a_cid, a_slot) = vm_with_a_mirror_slot(name);
    let (b, b_cid, b_slot) = vm_with_a_mirror_slot(name);
    assert_ne!(a_slot, b_slot, "each VM's slot is its own global ref");
    assert_eq!(ldc_global_slots(&a).lock().len(), 1);
    assert_eq!(ldc_global_slots(&b).lock().len(), 1);

    retire_ldc_slots_of_class(&a, a_cid);
    if LDC_SLOT_MEMO_SURVIVES_REDEFINITION {
        assert_eq!(ldc_global_slots(&a).lock().len(), 0);
    }
    assert_eq!(
        ldc_global_slots(&b).lock().len(),
        1,
        "another VM's retirement"
    );
    assert_eq!(class_mirror_slot_for(&b, b_cid), b_slot);
    drop(a);
    assert_eq!(
        class_mirror_slot_for(&b, b_cid),
        b_slot,
        "a dropped VM's teardown"
    );
}
