use super::*;

/// The CLDR candidates are APPENDED, least-specific first, and the plain
/// ones are kept.
///
/// Both halves matter. Keeping the plain chain means `TimeZoneNames`,
/// whose legacy root is the one `try_class_bundle` has always found,
/// still finds it. Appending means the CLDR bundle is instantiated LAST
/// and therefore returned, with the legacy root as its parent rather than
/// its replacement.
#[test]
fn the_cldr_packages_are_appended_after_the_plain_candidates() {
    let chain = vec![
        (
            "sun.util.resources.LocaleNames".to_string(),
            String::new(),
            String::new(),
        ),
        (
            "sun.util.resources.LocaleNames_en".to_string(),
            "en".to_string(),
            String::new(),
        ),
    ];
    let out = concrete_bundle_chain("sun.util.resources.LocaleNames", &chain);
    let names: Vec<&str> = out.iter().map(|(c, _, _)| c.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "sun.util.resources.LocaleNames",
            "sun.util.resources.LocaleNames_en",
            "sun.util.resources.cldr.LocaleNames",
            "sun.util.resources.cldr.ext.LocaleNames",
            "sun.util.resources.cldr.LocaleNames_en",
            "sun.util.resources.cldr.ext.LocaleNames_en",
        ]
    );
    // The locale each candidate stands for travels with it: `rb_get_bundle`
    // reads that pair back to stamp the bundle's `locale` field, and a
    // candidate that lost its language would stamp the root.
    assert_eq!(out[4].1, "en");
}

/// A base name with no CLDR package pair is handed back untouched rather
/// than given invented candidates.
#[test]
fn a_non_locale_base_name_keeps_its_own_chain() {
    let chain = vec![(
        "com.example.Messages".to_string(),
        String::new(),
        String::new(),
    )];
    let out = concrete_bundle_chain("com.example.Messages", &chain);
    assert_eq!(out, chain);
}

/// The two families this routing is for, and the one it is deliberately
/// not for.
#[test]
fn currency_names_is_not_routed_to_the_class_bundles() {
    assert!(needs_concrete_bundle_class(
        "sun.util.resources.TimeZoneNames",
        false
    ));
    assert!(needs_concrete_bundle_class(
        "sun.util.resources.LocaleNames",
        false
    ));
    // An APPLICATION caller asking for `LocaleNames` by hand gets
    // HotSpot's `MissingResourceException`, not our real bundle --
    // measured on `CurrencyNameProbe` rows 18 and 20.
    assert!(!needs_concrete_bundle_class(
        "sun.util.resources.LocaleNames",
        true
    ));
    // `TimeZoneNames` is unconditional, as it has been since 2026-08.
    assert!(needs_concrete_bundle_class(
        "sun.util.resources.TimeZoneNames",
        true
    ));
    // +38 armed on `L1LocaleProviderWorkload`: its curated path is doing
    // work the class bundles would have to replace, and that is its own
    // measurement rather than this one's corollary.
    assert!(!needs_concrete_bundle_class(
        "sun.util.resources.CurrencyNames",
        false
    ));
}
