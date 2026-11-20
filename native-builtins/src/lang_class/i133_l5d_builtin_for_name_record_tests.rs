use super::{builtin_for_name_kind_of_class, BuiltinForNameRecord};

/// Only the JDK's own two built-in loader classes have a row set; a
/// custom system loader, a user loader and a bare `URLClassLoader` ask
/// their `loadClass` every time.
#[test]
fn only_the_two_jdk_builtin_loader_classes_are_recorded() {
    assert_eq!(
        builtin_for_name_kind_of_class("jdk/internal/loader/ClassLoaders$PlatformClassLoader"),
        Some(0)
    );
    assert_eq!(
        builtin_for_name_kind_of_class("jdk/internal/loader/ClassLoaders$AppClassLoader"),
        Some(1)
    );
    for other in [
        "java/net/URLClassLoader",
        "java/lang/ClassLoader",
        "jdk/internal/loader/BuiltinClassLoader",
        "jdk/internal/loader/ClassLoaders$BootClassLoader",
        "app/CustomSystemLoader",
        "jdk/internal/loader/ClassLoaders$AppClassLoaderX",
    ] {
        assert_eq!(builtin_for_name_kind_of_class(other), None, "{other}");
    }
}

/// A row answers only its own loader and name; a re-filing replaces it;
/// a full loader's rows are cleared, the other loader's kept.
#[test]
fn rows_are_per_loader_and_bounded() {
    let mut record = BuiltinForNameRecord::default();
    assert_eq!(record.get(1, "java/util/Map"), None);
    record.insert(1, "java/util/Map", 7);
    assert_eq!(record.get(1, "java/util/Map"), Some(7));
    assert_eq!(record.get(0, "java/util/Map"), None, "the platform loader asked nothing");
    assert_eq!(record.get(1, "java/util/HashMap"), None);
    assert_eq!(record.get(2, "java/util/Map"), None, "no third row set");
    record.insert(2, "java/util/Map", 9);
    record.insert(1, "java/util/Map", 8);
    assert_eq!(record.get(1, "java/util/Map"), Some(8));
    record.insert(0, "java/sql/Date", 11);
    for i in 0..BuiltinForNameRecord::CAP {
        record.insert(1, &format!("p/C{i}"), i as u32);
    }
    // The CAP-th distinct name cleared the application rows before it
    // was filed; the platform row survives.
    assert_eq!(record.get(1, "java/util/Map"), None);
    let last = BuiltinForNameRecord::CAP - 1;
    assert_eq!(record.get(1, &format!("p/C{last}")), Some(last as u32));
    assert_eq!(record.get(0, "java/sql/Date"), Some(11));
}
