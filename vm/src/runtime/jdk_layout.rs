// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
mod tests;
