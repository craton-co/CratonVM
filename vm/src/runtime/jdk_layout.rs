// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDK home layout generator.
//!
//! Generates the directory tree and metadata files that make a CratonVM
//! installation recognizable as a JDK to Maven, Gradle, IntelliJ IDEA,
//! Eclipse, and the VS Code Java Extension Pack.
//!
//! The key artifacts are:
//! - `release` file (parsed by every build tool to detect the JDK version)
//! - `bin/` stubs (`java`, `javac`, etc.)
//! - `lib/jvm.cfg`, `lib/modules`, `lib/security/`, `conf/security/`
//! - `include/jni.h` and `include/jvmti.h`

use std::io;
use std::path::{Path, PathBuf};

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

const JAVA_VERSION: &str = "25";
const JAVA_VERSION_DATE: &str = "2025-09-16";
const IMPLEMENTOR: &str = "CratonVM";
const IMPLEMENTOR_VERSION: &str = "CratonVM 0.2.0";
const SOURCE: &str = ".:git:cratonvm";

/// Complete module list matching a full JDK 25 install.
const MODULES: &[&str] = &[
    "java.base",
    "java.compiler",
    "java.datatransfer",
    "java.desktop",
    "java.instrument",
    "java.logging",
    "java.management",
    "java.management.rmi",
    "java.naming",
    "java.net.http",
    "java.prefs",
    "java.rmi",
    "java.scripting",
    "java.se",
    "java.security.jgss",
    "java.security.sasl",
    "java.smartcardio",
    "java.sql",
    "java.sql.rowset",
    "java.transaction.xa",
    "java.xml",
    "java.xml.crypto",
    "jdk.accessibility",
    "jdk.attach",
    "jdk.charsets",
    "jdk.compiler",
    "jdk.crypto.cryptoki",
    "jdk.crypto.ec",
    "jdk.dynalink",
    "jdk.editpad",
    "jdk.hotspot.agent",
    "jdk.httpserver",
    "jdk.incubator.vector",
    "jdk.internal.ed",
    "jdk.internal.jvmstat",
    "jdk.internal.le",
    "jdk.internal.opt",
    "jdk.internal.vm.ci",
    "jdk.internal.vm.compiler",
    "jdk.jartool",
    "jdk.javadoc",
    "jdk.jcmd",
    "jdk.jconsole",
    "jdk.jdeps",
    "jdk.jdi",
    "jdk.jdwp.agent",
    "jdk.jfr",
    "jdk.jlink",
    "jdk.jpackage",
    "jdk.jshell",
    "jdk.jsobject",
    "jdk.jstatd",
    "jdk.localedata",
    "jdk.management",
    "jdk.management.agent",
    "jdk.management.jfr",
    "jdk.naming.dns",
    "jdk.naming.rmi",
    "jdk.net",
    "jdk.nio.mapmode",
    "jdk.random",
    "jdk.sctp",
    "jdk.security.auth",
    "jdk.security.jgss",
    "jdk.unsupported",
    "jdk.unsupported.desktop",
    "jdk.xml.dom",
    "jdk.zipfs",
];

/// Executables to place in `bin/`.
const BIN_TOOLS: &[&str] = &[
    "java", "javac", "javap", "jar", "jcmd", "jstack", "jmap", "jps", "jfr", "jshell",
];

// ─────────────────────────────────────────────────────────────────────────────
// ValidationResult
// ─────────────────────────────────────────────────────────────────────────────

/// Result of validating a single expected file or directory in the JDK layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationResult {
    /// Relative path from `jdk_home` that was checked.
    pub path: String,
    /// Whether the path exists.
    pub exists: bool,
    /// Human-readable note (e.g. "missing", "ok").
    pub note: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// JdkLayout
// ─────────────────────────────────────────────────────────────────────────────

/// Generates a JDK-compatible directory tree under `jdk_home`.
#[derive(Debug, Clone)]
pub struct JdkLayout {
    jdk_home: PathBuf,
}

impl JdkLayout {
    /// Create a new layout generator rooted at `jdk_home`.
    pub fn new(jdk_home: impl Into<PathBuf>) -> Self {
        Self {
            jdk_home: jdk_home.into(),
        }
    }

    /// Returns the root directory.
    pub fn jdk_home(&self) -> &Path {
        &self.jdk_home
    }

    // ── release file ────────────────────────────────────────────────────

    /// Write the `release` file that Maven/Gradle/IDEs parse to detect the
    /// JDK vendor and version.
    pub fn generate_release_file(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.jdk_home)?;

        let os_name = os_name_str();
        let os_arch = os_arch_str();
        // T6.6.1 requires MODULES as a comma-separated list. IntelliJ's
        // JdkUtil.parseReleaseFile accepts either space- or comma-separated
        // values inside the quoted string, but the roadmap pins the format
        // to comma-separated for determinism and readability.
        let modules_value = MODULES.join(",");

        let content = format!(
            "\
JAVA_VERSION=\"{JAVA_VERSION}\"\n\
JAVA_VERSION_DATE=\"{JAVA_VERSION_DATE}\"\n\
IMPLEMENTOR=\"{IMPLEMENTOR}\"\n\
IMPLEMENTOR_VERSION=\"{IMPLEMENTOR_VERSION}\"\n\
OS_ARCH=\"{os_arch}\"\n\
OS_NAME=\"{os_name}\"\n\
MODULES=\"{modules_value}\"\n\
SOURCE=\"{SOURCE}\"\n"
        );

        std::fs::write(self.jdk_home.join("release"), content)
    }

    // ── bin stubs ───────────────────────────────────────────────────────

    /// Create executable stubs in `bin/` for each tool.  On Unix the stubs
    /// are shell scripts; on Windows they are `.cmd` batch files.
    pub fn generate_bin_stubs(&self) -> io::Result<()> {
        let bin = self.jdk_home.join("bin");
        std::fs::create_dir_all(&bin)?;

        for tool in BIN_TOOLS {
            self.write_bin_stub(&bin, tool)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn write_bin_stub(&self, bin: &Path, name: &str) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let path = bin.join(name);
        let content = format!(
            "#!/bin/sh\n\
             # CratonVM stub for {name}\n\
             exec cratonvm --tool {name} \"$@\"\n"
        );
        std::fs::write(&path, content)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn write_bin_stub(&self, bin: &Path, name: &str) -> io::Result<()> {
        let path = bin.join(format!("{name}.cmd"));
        let content = format!(
            "@echo off\r\n\
             REM CratonVM stub for {name}\r\n\
             cratonvm --tool {name} %*\r\n"
        );
        std::fs::write(&path, content)?;
        Ok(())
    }

    // ── lib layout ──────────────────────────────────────────────────────

    /// Create `lib/jvm.cfg`, `lib/modules` marker, `lib/security/default.policy`,
    /// and `conf/security/java.security`.
    pub fn generate_lib_layout(&self) -> io::Result<()> {
        let lib = self.jdk_home.join("lib");
        let lib_security = lib.join("security");
        let conf_security = self.jdk_home.join("conf").join("security");
        std::fs::create_dir_all(&lib_security)?;
        std::fs::create_dir_all(&conf_security)?;

        // jvm.cfg — recognized by tools that probe lib/jvm.cfg
        let jvm_cfg = "\
-server KNOWN\n\
-client IGNORE\n";
        std::fs::write(lib.join("jvm.cfg"), jvm_cfg)?;

        // lib/modules — marker file (real JDKs have a jimage here)
        std::fs::write(lib.join("modules"), b"CRATONVM_MODULES_MARKER\n")?;

        // lib/security/default.policy
        let default_policy = "\
// CratonVM default security policy\n\
grant {\n\
    permission java.security.AllPermission;\n\
};\n";
        std::fs::write(lib_security.join("default.policy"), default_policy)?;

        // conf/security/java.security
        let java_security = "\
# CratonVM java.security properties\n\
security.provider.1=sun.security.provider.Sun\n\
securerandom.source=file:/dev/urandom\n\
keystore.type=pkcs12\n";
        std::fs::write(conf_security.join("java.security"), java_security)?;

        Ok(())
    }

    // ── include headers ─────────────────────────────────────────────────

    /// Create minimal `include/jni.h` and `include/jvmti.h` headers so that
    /// native builds that probe for JNI headers can locate them.
    pub fn generate_include_headers(&self) -> io::Result<()> {
        let include = self.jdk_home.join("include");
        std::fs::create_dir_all(&include)?;

        let jni_h = "\
/* CratonVM JNI header stub */\n\
#ifndef _JAVASOFT_JNI_H_\n\
#define _JAVASOFT_JNI_H_\n\
\n\
#include <stdarg.h>\n\
#include <stdint.h>\n\
\n\
typedef uint8_t  jboolean;\n\
typedef int8_t   jbyte;\n\
typedef uint16_t jchar;\n\
typedef int16_t  jshort;\n\
typedef int32_t  jint;\n\
typedef int64_t  jlong;\n\
typedef float    jfloat;\n\
typedef double   jdouble;\n\
typedef jint     jsize;\n\
\n\
typedef void* jobject;\n\
typedef jobject  jclass;\n\
typedef jobject  jstring;\n\
typedef jobject  jarray;\n\
typedef jobject  jthrowable;\n\
typedef void*    JNIEnv;\n\
typedef void*    JavaVM;\n\
\n\
#endif /* _JAVASOFT_JNI_H_ */\n";
        std::fs::write(include.join("jni.h"), jni_h)?;

        let jvmti_h = "\
/* CratonVM JVMTI header stub */\n\
#ifndef _JAVASOFT_JVMTI_H_\n\
#define _JAVASOFT_JVMTI_H_\n\
\n\
#include \"jni.h\"\n\
\n\
typedef void* jvmtiEnv;\n\
typedef jint  jvmtiError;\n\
\n\
#define JVMTI_VERSION_25 0x250000\n\
\n\
#endif /* _JAVASOFT_JVMTI_H_ */\n";
        std::fs::write(include.join("jvmti.h"), jvmti_h)?;

        Ok(())
    }

    // ── full setup ──────────────────────────────────────────────────────

    /// Generate the complete JDK home layout (release, bin, lib, include).
    pub fn setup_full_jdk_home(&self) -> io::Result<()> {
        self.generate_release_file()?;
        self.generate_bin_stubs()?;
        self.generate_lib_layout()?;
        self.generate_include_headers()?;
        Ok(())
    }

    // ── validation ──────────────────────────────────────────────────────

    /// Validate that every expected file/directory exists.  Returns one
    /// `ValidationResult` per check.
    pub fn validate_layout(&self) -> Vec<ValidationResult> {
        let expected: Vec<&str> = vec![
            "release",
            "bin",
            "lib/jvm.cfg",
            "lib/modules",
            "lib/security/default.policy",
            "conf/security/java.security",
            "include/jni.h",
            "include/jvmti.h",
        ];

        expected
            .into_iter()
            .map(|rel| {
                let full = self.jdk_home.join(rel);
                let exists = full.exists();
                ValidationResult {
                    path: rel.to_string(),
                    exists,
                    note: if exists {
                        "ok".to_string()
                    } else {
                        "missing".to_string()
                    },
                }
            })
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Platform helpers
// ─────────────────────────────────────────────────────────────────────────────

fn os_name_str() -> &'static str {
    if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "macos") {
        "Darwin"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Unknown"
    }
}

fn os_arch_str() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "amd64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86") {
        "x86"
    } else {
        "unknown"
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
}
