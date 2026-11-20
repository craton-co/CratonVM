use super::{DefinitionEpochs, NameEpochs, SiteCache, SITE_CACHES_SURVIVE_REDEFINITION};
use cratonvm_types::ClassId;

/// Fill `site` with `value`, retrying while concurrent tests move the
/// process-wide epochs under the fill; whether a fill was ever served.
fn fill<E: super::SiteEpochs>(
    table: &mut SiteCache<u32, E>,
    site: (ClassId, u16),
    value: u32,
) -> bool {
    for _ in 0..1000 {
        let epochs = SiteCache::<u32, E>::epochs_now();
        table.put(site.0, site.1, epochs, value);
        if table.get(site.0, site.1).copied() == Some(value) {
            return true;
        }
    }
    false
}

#[test]
fn a_redefinition_retires_older_entries_and_the_tables_keep_serving() {
    assert!(SITE_CACHES_SURVIVE_REDEFINITION);
    let site = (ClassId::new(0x1757), 9u16);
    let mut by_name: SiteCache<u32, NameEpochs> = SiteCache::new();
    let mut by_definition: SiteCache<u32, DefinitionEpochs> = SiteCache::new();
    assert!(fill(&mut by_name, site, 1));
    assert!(fill(&mut by_definition, site, 1));

    // What `redefine_class_with` does around the class swap (and the
    // resolution invalidate hook in between).
    crate::runtime::interpreter::bump_resolution_epoch();
    assert_eq!(
        by_name.get(site.0, site.1),
        None,
        "filled before the redefinition"
    );
    assert_eq!(
        by_definition.get(site.0, site.1),
        None,
        "filled before the redefinition"
    );

    // Filled after it: served, whatever the process-wide
    // `any_class_redefined` latch says (other tests may have raised it).
    assert!(
        fill(&mut by_name, site, 2),
        "the table keeps serving after a redefinition"
    );
    assert!(fill(&mut by_definition, site, 2));
}
