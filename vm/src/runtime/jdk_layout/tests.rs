use super::*;
use std::fs;

fn temp_jdk() -> (tempfile::TempDir, JdkLayout) {
    let dir = tempfile::tempdir().unwrap();
    let layout = JdkLayout::new(dir.path().join("jdk"));
    (dir, layout)
}

#[test]
fn release_file_contains_version() {
    let (_td, layout) = temp_jdk();
    layout.generate_release_file().unwrap();

    let content = fs::read_to_string(layout.jdk_home().join("release")).unwrap();
    assert!(content.contains("JAVA_VERSION=\"25\""));
    assert!(content.contains("IMPLEMENTOR=\"CratonVM\""));
    assert!(content.contains("IMPLEMENTOR_VERSION=\"CratonVM 0.2.0\""));
}

#[test]
fn release_file_contains_os_fields() {
    let (_td, layout) = temp_jdk();
    layout.generate_release_file().unwrap();

    let content = fs::read_to_string(layout.jdk_home().join("release")).unwrap();
    assert!(content.contains("OS_ARCH="));
    assert!(content.contains("OS_NAME="));
}

#[test]
fn release_file_contains_modules() {
    let (_td, layout) = temp_jdk();
    layout.generate_release_file().unwrap();

    let content = fs::read_to_string(layout.jdk_home().join("release")).unwrap();
    assert!(content.contains("java.base"));
    assert!(content.contains("jdk.compiler"));
}

/// T6.6.1: every key IntelliJ parses (JAVA_VERSION, JAVA_VERSION_DATE,
/// IMPLEMENTOR, IMPLEMENTOR_VERSION, OS_ARCH, OS_NAME, SOURCE, MODULES)
/// must be present in the `release` file.
#[test]
fn release_file_has_every_intellij_key() {
    let (_td, layout) = temp_jdk();
    layout.generate_release_file().unwrap();

    let content = fs::read_to_string(layout.jdk_home().join("release")).unwrap();
    for key in [
        "JAVA_VERSION=",
        "JAVA_VERSION_DATE=",
        "IMPLEMENTOR=",
        "IMPLEMENTOR_VERSION=",
        "OS_ARCH=",
        "OS_NAME=",
        "SOURCE=",
        "MODULES=",
    ] {
        assert!(
            content.contains(key),
            "release file missing required key: {key}\n---\n{content}"
        );
    }
}

/// T6.6.1: every key must have a quoted non-empty value.  IDEs reject
/// `KEY=` (unquoted) or `KEY=""`.
#[test]
fn release_file_values_are_quoted_and_non_empty() {
    let (_td, layout) = temp_jdk();
    layout.generate_release_file().unwrap();

    let content = fs::read_to_string(layout.jdk_home().join("release")).unwrap();
    for line in content.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        assert!(
            value.starts_with('"') && value.ends_with('"') && value.len() >= 2,
            "line not properly quoted: {line}"
        );
        let inner = &value[1..value.len() - 1];
        assert!(!inner.is_empty(), "key {key} has empty quoted value");
    }
}

/// T6.6.1 explicitly requires the MODULES list to be comma-separated.
#[test]
fn release_file_modules_are_comma_separated() {
    let (_td, layout) = temp_jdk();
    layout.generate_release_file().unwrap();

    let content = fs::read_to_string(layout.jdk_home().join("release")).unwrap();
    let line = content
        .lines()
        .find(|l| l.starts_with("MODULES="))
        .expect("MODULES line");
    // Extract the quoted value.
    let quoted = line.trim_start_matches("MODULES=");
    let inner = quoted.trim_matches('"');
    // Must contain at least one comma (it is a list).
    assert!(
        inner.contains(','),
        "MODULES must be comma-separated: {line}"
    );
    // Commas must NOT have surrounding spaces (real JDK format).
    assert!(
        !inner.contains(", ") && !inner.contains(" ,"),
        "MODULES comma separator must not include spaces: {line}"
    );
    // Sanity: java.base and jdk.compiler must both be listed.
    let parts: Vec<&str> = inner.split(',').collect();
    assert!(parts.contains(&"java.base"));
    assert!(parts.contains(&"jdk.compiler"));
}

/// T6.6.2: VS Code invokes `bin/java -version` during JDK probing, so
/// the stub must at least reference the `java` tool name.
#[test]
fn bin_java_stub_references_tool_name() {
    let (_td, layout) = temp_jdk();
    layout.generate_bin_stubs().unwrap();

    let path = if cfg!(unix) {
        layout.jdk_home().join("bin/java")
    } else {
        layout.jdk_home().join("bin/java.cmd")
    };
    let content = fs::read_to_string(path).unwrap();
    // The stub forwards "--tool java" to cratonvm so that `-version` is
    // dispatched correctly.
    assert!(
        content.contains("--tool java"),
        "java stub should dispatch --tool java: {content}"
    );
}

/// T6.6.3: Eclipse JDT reads `lib/modules` as the marker for a modular
/// runtime. It must exist even as a placeholder.
#[test]
fn lib_modules_marker_is_non_empty() {
    let (_td, layout) = temp_jdk();
    layout.generate_lib_layout().unwrap();

    let content = fs::read(layout.jdk_home().join("lib/modules")).unwrap();
    assert!(!content.is_empty(), "lib/modules must not be empty");
}

#[test]
fn bin_stubs_created() {
    let (_td, layout) = temp_jdk();
    layout.generate_bin_stubs().unwrap();

    let bin = layout.jdk_home().join("bin");
    assert!(bin.is_dir());

    for tool in BIN_TOOLS {
        if cfg!(unix) {
            assert!(bin.join(tool).exists(), "missing bin/{tool}");
        } else {
            let cmd_name = format!("{tool}.cmd");
            assert!(bin.join(&cmd_name).exists(), "missing bin/{cmd_name}");
        }
    }
}

#[test]
fn bin_stub_content_reasonable() {
    let (_td, layout) = temp_jdk();
    layout.generate_bin_stubs().unwrap();

    let bin = layout.jdk_home().join("bin");
    let path = if cfg!(unix) {
        bin.join("java")
    } else {
        bin.join("java.cmd")
    };
    let content = fs::read_to_string(path).unwrap();
    assert!(content.contains("cratonvm"));
    assert!(content.contains("java"));
}

#[test]
fn lib_layout_creates_files() {
    let (_td, layout) = temp_jdk();
    layout.generate_lib_layout().unwrap();

    assert!(layout.jdk_home().join("lib/jvm.cfg").exists());
    assert!(layout.jdk_home().join("lib/modules").exists());
    assert!(layout
        .jdk_home()
        .join("lib/security/default.policy")
        .exists());
    assert!(layout
        .jdk_home()
        .join("conf/security/java.security")
        .exists());
}

#[test]
fn jvm_cfg_content() {
    let (_td, layout) = temp_jdk();
    layout.generate_lib_layout().unwrap();

    let content = fs::read_to_string(layout.jdk_home().join("lib/jvm.cfg")).unwrap();
    assert!(content.contains("-server KNOWN"));
}

#[test]
fn include_headers_created() {
    let (_td, layout) = temp_jdk();
    layout.generate_include_headers().unwrap();

    let jni = layout.jdk_home().join("include/jni.h");
    let jvmti = layout.jdk_home().join("include/jvmti.h");
    assert!(jni.exists());
    assert!(jvmti.exists());

    let jni_content = fs::read_to_string(jni).unwrap();
    assert!(jni_content.contains("_JAVASOFT_JNI_H_"));
    assert!(jni_content.contains("typedef"));
}

#[test]
fn full_setup_creates_all() {
    let (_td, layout) = temp_jdk();
    layout.setup_full_jdk_home().unwrap();

    assert!(layout.jdk_home().join("release").exists());
    assert!(layout.jdk_home().join("bin").is_dir());
    assert!(layout.jdk_home().join("lib/jvm.cfg").exists());
    assert!(layout.jdk_home().join("include/jni.h").exists());
}

#[test]
fn validate_all_pass_after_full_setup() {
    let (_td, layout) = temp_jdk();
    layout.setup_full_jdk_home().unwrap();

    let results = layout.validate_layout();
    for r in &results {
        assert!(r.exists, "expected {}: {}", r.path, r.note);
        assert_eq!(r.note, "ok");
    }
}

#[test]
fn validate_detects_missing_before_setup() {
    let (_td, layout) = temp_jdk();
    // Do NOT call setup — everything should be missing.
    let results = layout.validate_layout();
    assert!(!results.is_empty());
    for r in &results {
        assert!(!r.exists);
        assert_eq!(r.note, "missing");
    }
}
