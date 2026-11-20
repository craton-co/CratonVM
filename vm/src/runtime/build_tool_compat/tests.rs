use super::*;

/// Helper: set up a minimal JDK skeleton for compat checks.
fn setup_minimal_jdk(root: &Path) {
    let jdk = root.join("jdk");
    std::fs::create_dir_all(jdk.join("bin")).unwrap();
    std::fs::create_dir_all(jdk.join("lib")).unwrap();
    std::fs::write(jdk.join("release"), "JAVA_VERSION=\"25\"\n").unwrap();
    if cfg!(unix) {
        std::fs::write(jdk.join("bin/java"), "#!/bin/sh\n").unwrap();
        std::fs::write(jdk.join("bin/javac"), "#!/bin/sh\n").unwrap();
    } else {
        std::fs::write(jdk.join("bin/java.cmd"), "@echo off\r\n").unwrap();
        std::fs::write(jdk.join("bin/javac.cmd"), "@echo off\r\n").unwrap();
    }
    std::fs::write(jdk.join("lib/jvm.cfg"), "-server KNOWN\n").unwrap();
}

// ── Maven tests ─────────────────────────────────────────────────────

#[test]
fn maven_valid_jdk_home() {
    let td = tempfile::tempdir().unwrap();
    setup_minimal_jdk(td.path());
    let checker = MavenCompatChecker::new(td.path().join("jdk"));
    assert_eq!(checker.check_java_home_valid(), CompatStatus::Ok);
}

#[test]
fn maven_missing_release() {
    let td = tempfile::tempdir().unwrap();
    let jdk = td.path().join("jdk");
    std::fs::create_dir_all(&jdk).unwrap();
    let checker = MavenCompatChecker::new(&jdk);
    assert!(matches!(
        checker.check_java_home_valid(),
        CompatStatus::Error(_)
    ));
}

#[test]
fn maven_missing_javac() {
    let td = tempfile::tempdir().unwrap();
    let jdk = td.path().join("jdk");
    std::fs::create_dir_all(jdk.join("lib")).unwrap();
    std::fs::write(jdk.join("release"), "JAVA_VERSION=\"25\"\n").unwrap();
    let checker = MavenCompatChecker::new(&jdk);
    assert!(matches!(
        checker.check_java_home_valid(),
        CompatStatus::Error(_)
    ));
}

#[test]
fn maven_toolchains_found() {
    let td = tempfile::tempdir().unwrap();
    let jdk = td.path().join("jdk");
    std::fs::create_dir_all(&jdk).unwrap();
    let tc_path = td.path().join("toolchains.xml");
    let jdk_str = jdk.to_string_lossy().to_string();
    std::fs::write(
        &tc_path,
        format!("<toolchains><jdkHome>{jdk_str}</jdkHome></toolchains>"),
    )
    .unwrap();
    let checker = MavenCompatChecker::new(&jdk);
    assert_eq!(checker.check_toolchains_xml(&tc_path), CompatStatus::Ok);
}

#[test]
fn maven_toolchains_not_found() {
    let td = tempfile::tempdir().unwrap();
    let checker = MavenCompatChecker::new(td.path().join("jdk"));
    let missing = td.path().join("no-such-file.xml");
    assert!(matches!(
        checker.check_toolchains_xml(&missing),
        CompatStatus::Warning(_)
    ));
}

#[test]
fn maven_toolchains_no_reference() {
    let td = tempfile::tempdir().unwrap();
    let tc_path = td.path().join("toolchains.xml");
    std::fs::write(&tc_path, "<toolchains></toolchains>").unwrap();
    let checker = MavenCompatChecker::new(td.path().join("jdk"));
    assert!(matches!(
        checker.check_toolchains_xml(&tc_path),
        CompatStatus::Warning(_)
    ));
}

#[test]
fn maven_settings_snippet_format() {
    let checker = MavenCompatChecker::new("/opt/cratonvm");
    let snippet = checker.generate_settings_snippet();
    assert!(snippet.contains("<toolchain>"));
    assert!(snippet.contains("<version>25</version>"));
    assert!(snippet.contains("<vendor>CratonVM</vendor>"));
    assert!(snippet.contains("/opt/cratonvm"));
}

/// Minimal structural XML validator.  Parses a string and verifies:
/// - every opening tag has a matching closing tag in the correct order
/// - tags are not orphaned
/// - self-closing tags are accepted
///
/// Returns the flat list of opening tag names in encounter order.
/// Used by Maven toolchains.xml roundtrip test below.
fn validate_xml_wellformed(xml: &str) -> Result<Vec<String>, String> {
    let bytes = xml.as_bytes();
    let mut i = 0;
    let mut stack: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // Find the closing '>'.
        let end = match xml[i..].find('>') {
            Some(e) => i + e,
            None => return Err(format!("unterminated tag at {i}")),
        };
        let inner = &xml[i + 1..end];

        if inner.starts_with("!--") || inner.starts_with('?') {
            // Comment or processing instruction — skip.
            i = end + 1;
            continue;
        }

        if let Some(name) = inner.strip_prefix('/') {
            // Closing tag.
            let name = name.trim().to_string();
            match stack.pop() {
                Some(top) if top == name => {}
                Some(top) => {
                    return Err(format!(
                        "mismatched close: expected </{top}>, got </{name}>"
                    ));
                }
                None => return Err(format!("unexpected closing tag </{name}>")),
            }
        } else if inner.ends_with('/') {
            // Self-closing tag.
            let name = inner[..inner.len() - 1]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                return Err("empty self-closing tag".into());
            }
            seen.push(name);
        } else {
            // Opening tag.
            let name = inner.split_whitespace().next().unwrap_or("").to_string();
            if name.is_empty() {
                return Err("empty opening tag".into());
            }
            stack.push(name.clone());
            seen.push(name);
        }

        i = end + 1;
    }

    if !stack.is_empty() {
        return Err(format!("unclosed tags at EOF: {stack:?}"));
    }
    Ok(seen)
}

/// T6.5.1 / T6.5.3: the snippet written to `toolchains.xml` must be
/// well-formed XML that Maven's Plexus DOM parser will accept.  We wrap
/// it in the `<toolchains>` root that Maven requires and validate that
/// the result parses cleanly and exposes the expected tags.
#[test]
fn maven_snippet_is_parseable_xml_roundtrip() {
    let checker = MavenCompatChecker::new("/opt/cratonvm");
    let snippet = checker.generate_settings_snippet();

    // Maven reads `<toolchains>...</toolchains>`; the snippet is one
    // `<toolchain>` entry, so wrap it before parsing.
    let doc = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <toolchains>\n{snippet}</toolchains>\n"
    );

    let tags = validate_xml_wellformed(&doc).expect("well-formed XML");
    // Every element Maven's <toolchain> schema requires must appear.
    for required in [
        "toolchains",
        "toolchain",
        "type",
        "provides",
        "version",
        "vendor",
        "configuration",
        "jdkHome",
    ] {
        assert!(
            tags.iter().any(|t| t == required),
            "parsed XML missing required tag: {required}\ntags={tags:?}"
        );
    }

    // And the round-tripped document must still contain the JDK home
    // path the user passed in (no escaping corruption).
    assert!(doc.contains("/opt/cratonvm"));
}

/// Paths with characters that would break naive string concatenation
/// (e.g. angle brackets) should either be escaped or detected — at
/// minimum, the resulting snippet should still be well-formed XML.
#[test]
fn maven_snippet_wellformed_for_windows_path() {
    let checker = MavenCompatChecker::new("C:\\Program Files\\cratonvm");
    let snippet = checker.generate_settings_snippet();
    let doc = format!("<toolchains>\n{snippet}</toolchains>\n");
    validate_xml_wellformed(&doc).expect("well-formed XML with Windows path");
}

#[test]
fn xml_validator_rejects_mismatched_tags() {
    assert!(validate_xml_wellformed("<a><b></a></b>").is_err());
}

#[test]
fn xml_validator_rejects_unclosed_tags() {
    assert!(validate_xml_wellformed("<a><b></b>").is_err());
}

#[test]
fn xml_validator_accepts_self_closing() {
    let tags = validate_xml_wellformed("<root><empty/></root>").unwrap();
    assert_eq!(tags, vec!["root", "empty"]);
}

// ── Gradle tests ────────────────────────────────────────────────────

#[test]
fn gradle_valid_jdk_home() {
    let td = tempfile::tempdir().unwrap();
    setup_minimal_jdk(td.path());
    let checker = GradleCompatChecker::new(td.path().join("jdk"));
    assert_eq!(checker.check_java_home_valid(), CompatStatus::Ok);
}

#[test]
fn gradle_missing_release() {
    let td = tempfile::tempdir().unwrap();
    let jdk = td.path().join("jdk");
    std::fs::create_dir_all(&jdk).unwrap();
    let checker = GradleCompatChecker::new(&jdk);
    assert!(matches!(
        checker.check_java_home_valid(),
        CompatStatus::Error(_)
    ));
}

#[test]
fn gradle_missing_java_bin() {
    let td = tempfile::tempdir().unwrap();
    let jdk = td.path().join("jdk");
    std::fs::create_dir_all(&jdk).unwrap();
    std::fs::write(jdk.join("release"), "JAVA_VERSION=\"25\"\n").unwrap();
    let checker = GradleCompatChecker::new(&jdk);
    assert!(matches!(
        checker.check_java_home_valid(),
        CompatStatus::Error(_)
    ));
}

#[test]
fn gradle_properties_format() {
    let checker = GradleCompatChecker::new("/opt/cratonvm");
    let props = checker.generate_gradle_properties();
    assert!(props.contains("org.gradle.java.home=/opt/cratonvm"));
}

#[test]
fn gradle_properties_normalizes_backslashes() {
    let checker = GradleCompatChecker::new("C:\\Program Files\\cratonvm");
    let props = checker.generate_gradle_properties();
    assert!(
        props.contains("C:/Program Files/cratonvm"),
        "backslashes should be normalized: {props}"
    );
    assert!(!props.contains('\\'), "no backslashes expected: {props}");
}

#[test]
fn gradle_toolchain_spec() {
    let checker = GradleCompatChecker::new("/opt/cratonvm");
    let spec = checker.generate_toolchain_spec();
    assert!(spec.contains("languageVersion.set(JavaLanguageVersion.of(25))"));
    assert!(spec.contains("CratonVM"));
}

#[test]
fn write_snippet_helper() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("nested/dir/snippet.txt");
    write_snippet_to_file(&path, "hello").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
}

#[test]
fn compat_status_is_ok() {
    assert!(CompatStatus::Ok.is_ok());
    assert!(!CompatStatus::Warning("w".into()).is_ok());
    assert!(!CompatStatus::Error("e".into()).is_ok());
}
