use super::{
    chm32_table_len, chm_bucket, is_glob_resource_name, java_string_hash,
    url_names_literal_resource,
};

/// Interpreter round i5 wave 100, lane L3: a `*` / `?` resource name is
/// literal; only a URL that names it exactly survives the filter.
#[test]
fn glob_names_keep_only_literal_urls() {
    assert!(is_glob_resource_name("p/*.txt"));
    assert!(is_glob_resource_name("a?b"));
    assert!(!is_glob_resource_name("p/a.txt"));
    assert!(url_names_literal_resource("file:/C:/cp/p/*.txt", "p/*.txt"));
    assert!(url_names_literal_resource("jar:file:/C:/a.jar!/p/*.txt", "p/*.txt"));
    assert!(url_names_literal_resource("file:/cp/*.txt", "*.txt"));
    assert!(!url_names_literal_resource("file:/C:/cp/p/one.txt", "p/*.txt"));
    assert!(!url_names_literal_resource("file:/C:/cp/xp/*.txt", "p/*.txt"));
}

/// Wave 99's `module-info.class` order: `String.hashCode`, the
/// `ConcurrentHashMap<>(32)` table (64 bins, doubled at 48 entries) and
/// its bin, checked against values computed outside the VM.
#[test]
fn chm_bins_follow_the_jdk() {
    assert_eq!(java_string_hash("hello"), 99_162_322);
    assert_eq!(java_string_hash("java.rmi"), -582_849_598);
    assert_eq!(chm32_table_len(0), 64);
    assert_eq!(chm32_table_len(47), 64);
    assert_eq!(chm32_table_len(48), 128);
    assert_eq!(chm_bucket("java.rmi", 64), 0);
    assert_eq!(chm_bucket("jdk.management.jfr", 64), 0);
    assert_eq!(chm_bucket("java.base", 64), 30);
    assert_eq!(chm_bucket("hello", 64), 59);
}
