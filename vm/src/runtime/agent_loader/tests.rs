use super::*;

#[test]
fn parse_manifest_premain_only() {
    let manifest_text = "Manifest-Version: 1.0\r\nPremain-Class: foo.Bar\r\n";
    let info = ManifestInfo::parse(manifest_text.as_bytes());
    let agent =
        parse_manifest_attributes(Path::new("/tmp/x.jar"), &info, Some("opt1=foo".into()))
            .expect("parse_manifest_attributes");
    assert_eq!(agent.premain_class, "foo.Bar");
    assert_eq!(agent.agent_args.as_deref(), Some("opt1=foo"));
    assert!(!agent.can_redefine);
    assert!(!agent.can_retransform);
    assert!(!agent.can_set_native_method_prefix);
    assert!(agent.boot_class_path.is_empty());
}

#[test]
fn parse_manifest_full_attributes() {
    let manifest_text = "Manifest-Version: 1.0\r\n\
         Premain-Class: com.example.Agent\r\n\
         Agent-Class: com.example.Agent\r\n\
         Boot-Class-Path: lib/x.jar lib/y.jar\r\n\
         Can-Redefine-Classes: true\r\n\
         Can-Retransform-Classes: TRUE\r\n\
         Can-Set-Native-Method-Prefix: false\r\n";
    let info = ManifestInfo::parse(manifest_text.as_bytes());
    let agent = parse_manifest_attributes(Path::new("/opt/agents/agent.jar"), &info, None)
        .expect("parse_manifest_attributes");

    assert_eq!(agent.premain_class, "com.example.Agent");
    assert_eq!(agent.agent_class.as_deref(), Some("com.example.Agent"));
    assert!(agent.can_redefine);
    assert!(agent.can_retransform);
    assert!(!agent.can_set_native_method_prefix);
    // Boot-Class-Path entries are resolved relative to the JAR's parent.
    assert_eq!(agent.boot_class_path.len(), 2);
    assert!(
        agent.boot_class_path[0].ends_with("lib/x.jar")
            || agent.boot_class_path[0].ends_with("lib\\x.jar")
    );
    assert!(
        agent.boot_class_path[1].ends_with("lib/y.jar")
            || agent.boot_class_path[1].ends_with("lib\\y.jar")
    );
}

#[test]
fn parse_manifest_missing_premain_class_errors() {
    let manifest_text = "Manifest-Version: 1.0\r\nAgent-Class: x.Y\r\n";
    let info = ManifestInfo::parse(manifest_text.as_bytes());
    let err = parse_manifest_attributes(Path::new("/tmp/x.jar"), &info, None)
        .expect_err("missing Premain-Class should error");
    assert!(matches!(err, AgentLoadError::MissingPremainClass { .. }));
}

#[test]
fn parse_manifest_empty_premain_class_errors() {
    let manifest_text = "Manifest-Version: 1.0\r\nPremain-Class: \r\n";
    let info = ManifestInfo::parse(manifest_text.as_bytes());
    let err = parse_manifest_attributes(Path::new("/tmp/x.jar"), &info, None)
        .expect_err("empty Premain-Class should error");
    assert!(matches!(err, AgentLoadError::MissingPremainClass { .. }));
}

#[test]
fn parse_javaagent_spec_bad_prefix() {
    let err =
        parse_javaagent_spec("-Xfoo:bar.jar").expect_err("non -javaagent: arg should fail");
    assert!(matches!(err, AgentLoadError::BadPrefix(_)));
}

#[test]
fn parse_javaagent_spec_missing_jar_file() {
    // Using a non-existent jar should produce JarNotFound.
    let err = parse_javaagent_spec("-javaagent:/nonexistent/path/definitely-not-here.jar")
        .expect_err("missing jar should fail");
    assert!(matches!(err, AgentLoadError::JarNotFound { .. }));
}

#[test]
fn boolean_attribute_parses_case_insensitively() {
    let mut attrs = std::collections::HashMap::new();
    attrs.insert("A".to_string(), "true".to_string());
    attrs.insert("B".to_string(), "TRUE".to_string());
    attrs.insert("C".to_string(), "True".to_string());
    attrs.insert("D".to_string(), "false".to_string());
    attrs.insert("E".to_string(), "garbage".to_string());

    assert!(boolean_attribute(&attrs, "A"));
    assert!(boolean_attribute(&attrs, "B"));
    assert!(boolean_attribute(&attrs, "C"));
    assert!(!boolean_attribute(&attrs, "D"));
    assert!(!boolean_attribute(&attrs, "E"));
    assert!(!boolean_attribute(&attrs, "Missing"));
}

/// Parse a real `-javaagent:foo.jar=opts` argument by writing a
/// synthesised JAR to a tempdir; verifies the round-trip from a
/// CLI-style string all the way through `LoadedAgent` extraction.
#[test]
fn parse_javaagent_spec_full_round_trip() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    let tmp = tempfile::tempdir().expect("tempdir");
    let jar_path = tmp.path().join("test-agent.jar");

    // Synthesise a JAR with just MANIFEST.MF.
    let manifest_text = "Manifest-Version: 1.0\r\n\
         Premain-Class: com.example.MyAgent\r\n\
         Boot-Class-Path: lib/extra.jar\r\n\
         Can-Redefine-Classes: true\r\n";
    {
        let file = std::fs::File::create(&jar_path).expect("create jar");
        let mut zip = ZipWriter::new(file);
        let opts: SimpleFileOptions =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("META-INF/MANIFEST.MF", opts)
            .expect("start file");
        zip.write_all(manifest_text.as_bytes())
            .expect("write manifest");
        zip.finish().expect("finish zip");
    }

    let spec = format!("-javaagent:{}=key=value,opt2", jar_path.to_string_lossy());
    let agent = parse_javaagent_spec(&spec).expect("round-trip ok");
    assert_eq!(agent.premain_class, "com.example.MyAgent");
    assert_eq!(agent.agent_args.as_deref(), Some("key=value,opt2"));
    assert!(agent.can_redefine);
    assert!(!agent.can_retransform);
    assert_eq!(agent.boot_class_path.len(), 1);
    assert!(
        agent.boot_class_path[0].ends_with("lib/extra.jar")
            || agent.boot_class_path[0].ends_with("lib\\extra.jar")
    );
    assert_eq!(agent.jar_path, jar_path);
}

/// `-javaagent:foo.jar` with no `=opts` should still parse, with
/// `agent_args` left as `None` so that the dispatcher sends an
/// empty string to the Java side.
#[test]
fn parse_javaagent_spec_no_opts() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    let tmp = tempfile::tempdir().expect("tempdir");
    let jar_path = tmp.path().join("noopts.jar");

    let manifest_text = "Manifest-Version: 1.0\r\nPremain-Class: x.Y\r\n";
    {
        let file = std::fs::File::create(&jar_path).expect("create jar");
        let mut zip = ZipWriter::new(file);
        let opts: SimpleFileOptions =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("META-INF/MANIFEST.MF", opts)
            .expect("start file");
        zip.write_all(manifest_text.as_bytes())
            .expect("write manifest");
        zip.finish().expect("finish zip");
    }

    let spec = format!("-javaagent:{}", jar_path.to_string_lossy());
    let agent = parse_javaagent_spec(&spec).expect("ok");
    assert_eq!(agent.premain_class, "x.Y");
    assert!(agent.agent_args.is_none());
}

/// `=` inside the agent-arg portion must be preserved verbatim.
#[test]
fn parse_javaagent_spec_preserves_equals_in_args() {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    let tmp = tempfile::tempdir().expect("tempdir");
    let jar_path = tmp.path().join("args-with-equals.jar");

    let manifest_text = "Manifest-Version: 1.0\r\nPremain-Class: a.B\r\n";
    {
        let file = std::fs::File::create(&jar_path).expect("create jar");
        let mut zip = ZipWriter::new(file);
        let opts: SimpleFileOptions =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("META-INF/MANIFEST.MF", opts)
            .expect("start file");
        zip.write_all(manifest_text.as_bytes())
            .expect("write manifest");
        zip.finish().expect("finish zip");
    }

    // First `=` separates path from args; subsequent `=` are agent-side.
    let spec = format!("-javaagent:{}=k1=v1,k2=v2", jar_path.to_string_lossy());
    let agent = parse_javaagent_spec(&spec).expect("ok");
    assert_eq!(agent.agent_args.as_deref(), Some("k1=v1,k2=v2"));
}

/// Empty agent list — invoke_premains should be a no-op success.
/// Note: full premain dispatch can't be tested here without a
/// full VM bootstrap; that lives in the wave2-4 integration tests
/// (Owner D). What we DO test here is that the parser produces the
/// LoadedAgent values that the dispatcher reads.
#[test]
fn loaded_agent_roundtrip_fields() {
    let a = LoadedAgent {
        jar_path: PathBuf::from("/tmp/x.jar"),
        agent_args: Some("o".into()),
        premain_class: "com.x.Y".into(),
        agent_class: Some("com.x.Y".into()),
        can_redefine: true,
        can_retransform: false,
        can_set_native_method_prefix: true,
        boot_class_path: vec![PathBuf::from("/tmp/lib/a.jar")],
    };
    // The struct is Clone + Debug — no panic on either path.
    let cloned = a.clone();
    assert_eq!(cloned.premain_class, "com.x.Y");
    assert!(cloned.can_redefine);
    let _ = format!("{a:?}");
}

/// `Boot-Class-Path` goes to the bootstrap search in manifest order and
/// the agent JAR to the application class path; the kill-switch arm
/// keeps the old all-application split (round 13 wave 11, lane misc11).
#[test]
fn boot_class_path_entries_go_to_the_bootstrap_search() {
    let a = LoadedAgent {
        jar_path: PathBuf::from("agent.jar"),
        agent_args: None,
        premain_class: "a.B".into(),
        agent_class: None,
        can_redefine: false,
        can_retransform: false,
        can_set_native_method_prefix: false,
        boot_class_path: vec![PathBuf::from("h1.jar"), PathBuf::from("h2.jar")],
    };
    let (boot, app) = agent_search_paths(&a, true);
    assert_eq!(boot, vec!["h1.jar".to_string(), "h2.jar".to_string()]);
    assert_eq!(app, vec!["agent.jar".to_string()]);

    let (boot, app) = agent_search_paths(&a, false);
    assert!(boot.is_empty());
    assert_eq!(
        app,
        vec![
            "h1.jar".to_string(),
            "h2.jar".to_string(),
            "agent.jar".to_string()
        ]
    );
}
