use super::{nested_jar_spec_parts, nested_jar_walk_name_is_plain, percent_decode_utf8};

/// Spring Boot's `JarUrl.create(file, "BOOT-INF/lib/x.jar")` base spec
/// splits into the outer path and the inner jar; anything else (a
/// directory entry, a plain `file:` jar, an ambiguous `!` / `%`) does not.
#[test]
fn nested_jar_spec_parts_accepts_only_springs_nested_jar_form() {
    assert_eq!(
        nested_jar_spec_parts("nested:/data/app%20dir/app.jar/!BOOT-INF/lib/x-1.0.jar!/"),
        Some((
            "/data/app dir/app.jar".to_string(),
            "BOOT-INF/lib/x-1.0.jar".to_string()
        ))
    );
    assert_eq!(
        nested_jar_spec_parts("nested:/C:/apps/app.jar/!BOOT-INF/lib/x.jar!/"),
        Some((
            "C:/apps/app.jar".to_string(),
            "BOOT-INF/lib/x.jar".to_string()
        ))
    );
    for spec in [
        "nested:/data/app.jar/!BOOT-INF/classes/!/",
        "file:/data/app.jar!/",
        "nested:/data/app.jar/!BOOT-INF/lib/x.jar",
        "nested:/data/app.jar/!BOOT-INF/lib/x!y.jar!/",
        "nested:/data/app.jar/!BOOT-INF/lib/x%21.jar!/",
        "nested:/da!ta/app.jar/!BOOT-INF/lib/x.jar!/",
        "nested:/!BOOT-INF/lib/x.jar!/",
        "nested:/data/app.jar/!!/",
        "nested:/data/%ZZ/app.jar/!BOOT-INF/lib/x.jar!/",
    ] {
        assert_eq!(nested_jar_spec_parts(spec), None, "{spec}");
    }
}

#[test]
fn percent_decode_is_utf8() {
    assert_eq!(
        percent_decode_utf8("/a%20b/%C3%A9.jar").as_deref(),
        Some("/a b/\u{e9}.jar")
    );
    assert_eq!(percent_decode_utf8("/plain").as_deref(), Some("/plain"));
    assert_eq!(percent_decode_utf8("/bad%2").as_deref(), None);
    assert_eq!(percent_decode_utf8("/bad%FF").as_deref(), None);
}

/// Only a name whose URL is exactly `<base><name>` may be answered from
/// the index: the JDK body and Spring's handler would resolve the others
/// to some other entry (or none).
#[test]
fn walk_names_must_be_plain() {
    for name in ["p/A.class", "META-INF/spring.factories", "a$b-c_d.e+f", "x"] {
        assert!(nested_jar_walk_name_is_plain(name), "{name}");
    }
    for name in [
        "",
        "/p/A.class",
        "dir/",
        "a//b",
        "./a",
        "a/../b",
        "a/.",
        "a#b",
        "a?b",
        "a:b",
        "a%20b",
        "a!/b",
        "a\\b",
        "a b",
        "\u{e9}.txt",
    ] {
        assert!(!nested_jar_walk_name_is_plain(name), "{name:?}");
    }
}
