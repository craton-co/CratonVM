use super::{DefinitionEpochs, NameEpochs, SiteCache, SiteEpochs, STORE_TAG};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;

fn slot_of(shared: &SharedVm) -> usize {
    cratonvm_classloading::store_epoch_slot(shared.jit.class_layout_domain)
}

/// Fill `site` with `value` under `shared`'s counters, retrying while
/// concurrent tests move them under the fill; the stamp that stuck.
fn fill<E: SiteEpochs>(
    table: &mut SiteCache<u32, E>,
    shared: &SharedVm,
    site: (ClassId, u16),
    value: u32,
) -> Option<(u64, u64)> {
    for _ in 0..1000 {
        let stamp = SiteCache::<u32, E>::epochs_for(shared);
        table.put(site.0, site.1, stamp, value);
        if table.get(site.0, site.1).copied() == Some(value) {
            return Some(stamp);
        }
    }
    None
}

#[test]
fn a_vm_stamp_carries_its_slot() {
    let shared = SharedVm::new(VmConfig::default());
    let stamp = SiteCache::<u32, NameEpochs>::epochs_for(&shared);
    assert_ne!(stamp.1 & STORE_TAG, 0);
    assert_eq!(
        ((stamp.1 & !STORE_TAG) >> super::STORE_SLOT_SHIFT) as usize,
        slot_of(&shared)
    );
    // The process stamp is untagged.
    assert_eq!(SiteCache::<u32, NameEpochs>::epochs_now().1 & STORE_TAG, 0);
}

#[test]
fn another_vms_resolution_bump_leaves_this_vms_entries() {
    let a = SharedVm::new(VmConfig::default());
    let b = SharedVm::new(VmConfig::default());
    let site = (ClassId::new(0x2020), 4u16);
    let mut by_name: SiteCache<u32, NameEpochs> = SiteCache::new();
    let mut by_definition: SiteCache<u32, DefinitionEpochs> = SiteCache::new();
    let name_stamp = fill(&mut by_name, &b, site, 7).expect("a fill sticks");
    let definition_stamp = fill(&mut by_definition, &b, site, 8).expect("a fill sticks");

    crate::runtime::interpreter::bump_resolution_epoch_in(&a);
    // Slots are shared modulo the table size, and a store another test
    // builds concurrently may share B's: the survival claim is asserted
    // only while B's own counters stayed put.
    let name_hit = by_name.get(site.0, site.1).copied();
    let definition_hit = by_definition.get(site.0, site.1).copied();
    if slot_of(&a) != slot_of(&b) {
        if SiteCache::<u32, NameEpochs>::epochs_for(&b) == name_stamp {
            assert_eq!(name_hit, Some(7), "A's bump retired a B entry");
        }
        if SiteCache::<u32, DefinitionEpochs>::epochs_for(&b) == definition_stamp {
            assert_eq!(definition_hit, Some(8), "A's bump retired a B entry");
        }
    }

    // B's own bump retires both, always.
    crate::runtime::interpreter::bump_resolution_epoch_in(&b);
    assert_eq!(by_name.get(site.0, site.1), None);
    assert_eq!(by_definition.get(site.0, site.1), None);
}

#[test]
fn a_class_definition_retires_only_its_own_vms_definition_entries() {
    let a = SharedVm::new(VmConfig::default());
    let b = SharedVm::new(VmConfig::default());
    let site = (ClassId::new(0x2021), 5u16);
    let mut for_a: SiteCache<u32, DefinitionEpochs> = SiteCache::new();
    let mut for_b: SiteCache<u32, DefinitionEpochs> = SiteCache::new();
    fill(&mut for_a, &a, site, 1).expect("a fill sticks");
    let b_stamp = fill(&mut for_b, &b, site, 2).expect("a fill sticks");

    a.classes.class_manager_write().register_class_name(
        cratonvm_types::ClassLoaderId::Application,
        "w20l2/DefinedInA",
        ClassId::new(900_301),
    );
    assert_eq!(
        for_a.get(site.0, site.1),
        None,
        "A's definition moves A's slot"
    );
    let b_hit = for_b.get(site.0, site.1).copied();
    if slot_of(&a) != slot_of(&b)
        && SiteCache::<u32, DefinitionEpochs>::epochs_for(&b) == b_stamp
    {
        assert_eq!(b_hit, Some(2), "A's definition retired a B entry");
    }
}

/// The cast-site memo epoch of a VM is tagged (never equal to a process
/// snapshot) and moves with that VM's class definitions.
#[test]
fn a_vm_cast_memo_epoch_is_tagged_and_moves_with_its_store() {
    let a = SharedVm::new(VmConfig::default());
    let before = super::cast_memo_epoch_in(&a);
    assert_ne!(before & STORE_TAG, 0);
    assert_ne!(before, super::cast_memo_epoch_now());
    a.classes.class_manager_write().register_class_name(
        cratonvm_types::ClassLoaderId::Application,
        "w20l2/MemoEpochProbe",
        ClassId::new(900_302),
    );
    assert_ne!(super::cast_memo_epoch_in(&a), before);
}

/// An entry stamped with the process pair is retired by any VM's bump:
/// the fallback stays conservative.
#[test]
fn a_process_stamped_entry_is_retired_by_any_vms_bump() {
    let a = SharedVm::new(VmConfig::default());
    let site = (ClassId::new(0x2022), 6u16);
    let mut table: SiteCache<u32, NameEpochs> = SiteCache::new();
    let mut filled = false;
    for _ in 0..1000 {
        table.put(
            site.0,
            site.1,
            SiteCache::<u32, NameEpochs>::epochs_now(),
            3,
        );
        if table.get(site.0, site.1).copied() == Some(3) {
            filled = true;
            break;
        }
    }
    assert!(filled);
    crate::runtime::interpreter::bump_resolution_epoch_in(&a);
    assert_eq!(table.get(site.0, site.1), None);
}
