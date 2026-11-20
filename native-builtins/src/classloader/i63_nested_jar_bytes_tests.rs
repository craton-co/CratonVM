use super::{nested_jar_resource_served_len, nested_jar_url_entry_parts};

#[test]
fn url_file_splits_into_the_nested_spec_and_the_entry() {
    assert_eq!(
        nested_jar_url_entry_parts("nested:/data/app.jar/!BOOT-INF/lib/a.jar!/p/A.class"),
        Some(("nested:/data/app.jar/!BOOT-INF/lib/a.jar!/", "p/A.class"))
    );
    assert_eq!(
        nested_jar_url_entry_parts("nested:/C:/x%20y/app.jar/!BOOT-INF/lib/a.jar!/A.class"),
        Some(("nested:/C:/x%20y/app.jar/!BOOT-INF/lib/a.jar!/", "A.class"))
    );
    // No entry, a directory spec, a `!` in the outer path, not nested,
    // no `!/` after the inner jar.
    for file in [
        "nested:/data/app.jar/!BOOT-INF/lib/a.jar!/",
        "nested:/data/app.jar/!BOOT-INF/classes/!/p/A.class",
        "nested:/da!ta/app.jar/!BOOT-INF/lib/a.jar!/p/A.class",
        "file:/data/app.jar/!BOOT-INF/lib/a.jar!/p/A.class",
        "nested:/data/app.jar/!BOOT-INF/lib/a.jar",
    ] {
        assert_eq!(nested_jar_url_entry_parts(file), None, "{file}");
    }
}

/// `Resource.getBytes`'s JDK 25 loop over a stream of exactly 10 bytes.
#[test]
fn served_length_follows_the_body_loop() {
    assert_eq!(nested_jar_resource_served_len(-1, 10), (10, false));
    assert_eq!(nested_jar_resource_served_len(i32::MAX, 10), (10, false));
    assert_eq!(nested_jar_resource_served_len(10, 10), (10, false));
    assert_eq!(nested_jar_resource_served_len(4, 10), (4, false));
    assert_eq!(nested_jar_resource_served_len(0, 10), (0, false));
    assert_eq!(nested_jar_resource_served_len(-2, 10), (0, false));
    assert_eq!(nested_jar_resource_served_len(11, 10), (10, true));
    assert_eq!(nested_jar_resource_served_len(-1, 0), (0, false));
}
