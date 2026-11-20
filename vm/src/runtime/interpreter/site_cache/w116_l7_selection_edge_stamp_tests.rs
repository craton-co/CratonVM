use super::{
    DefinitionEpochs, SelectionEpochs, SiteCache, SELECTION_EDGE_TAG, STORE_SLOT_BITS,
    STORE_SLOT_SHIFT, STORE_TAG,
};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;

#[test]
fn a_selection_edge_stamp_carries_its_mode_and_is_read_back() {
    let shared = SharedVm::new(VmConfig::default());
    let slot = cratonvm_classloading::store_epoch_slot(shared.jit.class_layout_domain);
    let stamp = SiteCache::<u32, SelectionEpochs>::epochs_for_selection(&shared, true);
    assert_ne!(stamp.1 & STORE_TAG, 0);
    assert_ne!(stamp.1 & SELECTION_EDGE_TAG, 0);
    assert_eq!(((stamp.1 >> STORE_SLOT_SHIFT) & STORE_SLOT_BITS) as usize, slot);
    let plain = SiteCache::<u32, SelectionEpochs>::epochs_for_selection(&shared, false);
    assert_eq!(plain.1 & SELECTION_EDGE_TAG, 0);
    // Filled under the edge stamp, read back (retrying while concurrent
    // tests move the counters under the fill).
    let site = (ClassId::new(0x1160), 3u16);
    let mut table: SiteCache<u32, SelectionEpochs> = SiteCache::new();
    let stuck = (0..1000).any(|_| {
        let stamp = SiteCache::<u32, SelectionEpochs>::epochs_for_selection(&shared, true);
        table.put(site.0, site.1, stamp, 9);
        table.get(site.0, site.1).copied() == Some(9)
    });
    assert!(stuck, "an edge-stamped fill is read back");
    // Without an edge mode of its own, a table answers its ordinary pair.
    let same = (0..1000).any(|_| {
        SiteCache::<u32, DefinitionEpochs>::epochs_for_selection(&shared, true).0
            == SiteCache::<u32, DefinitionEpochs>::epochs_for(&shared).0
    });
    assert!(same);
}
