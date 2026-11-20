use super::{DefinitionEpochs, NameEpochs, SiteCache, SiteEpochs};
use cratonvm_types::ClassId;

/// Stage 1a: the definition of a brand-new class name retires a
/// [`DefinitionEpochs`] entry (the definition epoch always moves) but not
/// a [`NameEpochs`] entry. The counters are process-wide and other tests
/// define classes concurrently, so the survival half is asserted only
/// over a window in which the name generation itself stayed put.
#[test]
fn a_new_class_name_retires_definition_tagged_entries_only() {
    let site = (ClassId::new(7), 3u16);
    let mut by_definition: SiteCache<u32, DefinitionEpochs> = SiteCache::new();
    let mut by_name: SiteCache<u32, NameEpochs> = SiteCache::new();
    let definition_before = DefinitionEpochs::now();
    let name_before = NameEpochs::now();
    by_definition.put(site.0, site.1, definition_before, 11);
    by_name.put(site.0, site.1, name_before, 22);

    let mut mgr = cratonvm_classloading::ClassManager::new(&[], &[], &[]);
    mgr.try_ensure_synthetic_class("cratonvm/test/i6l2/FreshName", 0)
        .expect("an absent name is fabricated in the default mode");

    assert_eq!(
        by_definition.get(site.0, site.1),
        None,
        "a definition moves the definition epoch"
    );
    if !cratonvm_classloading::any_class_redefined() && NameEpochs::now() == name_before {
        assert_eq!(by_name.get(site.0, site.1).copied(), Some(22));
    }
}
