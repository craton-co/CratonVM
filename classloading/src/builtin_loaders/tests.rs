use super::*;

#[test]
fn kind_roundtrip_for_known_names() {
    assert_eq!(
        builtin_loader_kind(BOOT_LOADER_STATIC_CLASS),
        Some(BuiltinLoaderKind::Boot)
    );
    assert_eq!(
        builtin_loader_kind(PLATFORM_LOADER_CLASS),
        Some(BuiltinLoaderKind::Platform)
    );
    assert_eq!(
        builtin_loader_kind(APP_LOADER_CLASS),
        Some(BuiltinLoaderKind::App)
    );
    assert_eq!(
        builtin_loader_kind(BUILTIN_LOADER_CLASS),
        Some(BuiltinLoaderKind::BuiltinBase)
    );
    assert_eq!(builtin_loader_kind("java/lang/ClassLoader"), None);
    assert_eq!(builtin_loader_kind("java/net/URLClassLoader"), None);
}

#[test]
fn parent_chain_is_app_platform_bootstrap() {
    assert_eq!(
        builtin_parent_class(BuiltinLoaderKind::App),
        Some(PLATFORM_LOADER_CLASS)
    );
    assert!(builtin_parent_class(BuiltinLoaderKind::Platform).is_none());
    assert!(builtin_parent_class(BuiltinLoaderKind::Boot).is_none());
}

#[test]
fn display_names_match_hotspot_convention() {
    assert_eq!(builtin_display_name(BuiltinLoaderKind::App), Some("app"));
    assert_eq!(
        builtin_display_name(BuiltinLoaderKind::Platform),
        Some("platform")
    );
    assert!(builtin_display_name(BuiltinLoaderKind::Boot).is_none());
}

#[test]
fn registration_hook_is_idempotent_and_counts() {
    let before = registration_count();
    register_builtin_loader_aliases();
    register_builtin_loader_aliases();
    let after = registration_count();
    assert!(after >= before + 2);
}

#[test]
fn all_kinds_includes_three_entries_in_order() {
    let kinds: Vec<_> = all_kinds().collect();
    assert_eq!(
        kinds,
        vec![
            BuiltinLoaderKind::Boot,
            BuiltinLoaderKind::Platform,
            BuiltinLoaderKind::App,
        ]
    );
}
