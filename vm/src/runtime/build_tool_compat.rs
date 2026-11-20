// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Build tool compatibility checks for Maven and Gradle.
//!
//! Provides checkers that verify a CratonVM installation can be used by
//! Maven and Gradle as a JDK, and helpers that generate the configuration
//! snippets needed for toolchain integration.

use std::io;
use std::path::{Path, PathBuf};

// ─────────────────────────────────────────────────────────────────────────────
// CompatStatus
// ─────────────────────────────────────────────────────────────────────────────

/// Outcome of a compatibility check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatStatus {
    /// Fully compatible.
    Ok,
    /// Something is missing or misconfigured.
    Warning(String),
    /// Unusable for the build tool.
    Error(String),
}

impl CompatStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, CompatStatus::Ok)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MavenCompatChecker
// ─────────────────────────────────────────────────────────────────────────────

/// Checks and helpers for Apache Maven compatibility.
#[derive(Debug, Clone)]
pub struct MavenCompatChecker {
    jdk_home: PathBuf,
}

impl MavenCompatChecker {
    pub fn new(jdk_home: impl Into<PathBuf>) -> Self {
        Self {
            jdk_home: jdk_home.into(),
        }
    }

    /// Verify that `jdk_home` looks like a valid JDK to Maven.
    ///
    /// Maven checks for `<jdk_home>/release`, `bin/javac` (or `.cmd`), and
    /// `lib/` presence.
    pub fn check_java_home_valid(&self) -> CompatStatus {
        if !self.jdk_home.join("release").is_file() {
            return CompatStatus::Error(
                "release file missing; Maven cannot detect JDK version".into(),
            );
        }

        let javac_exists = if cfg!(unix) {
            self.jdk_home.join("bin/javac").exists()
        } else {
            self.jdk_home.join("bin/javac.cmd").exists()
        };
        if !javac_exists {
            return CompatStatus::Error("bin/javac not found; Maven requires a compiler".into());
        }

        if !self.jdk_home.join("lib").is_dir() {
            return CompatStatus::Warning("lib/ directory missing".into());
        }

        CompatStatus::Ok
    }

    /// Check whether a Maven `toolchains.xml` at the given path already
    /// contains an entry pointing to this JDK home.
    pub fn check_toolchains_xml(&self, toolchains_path: &Path) -> CompatStatus {
        let content = match std::fs::read_to_string(toolchains_path) {
            Ok(c) => c,
            Err(_) => {
                return CompatStatus::Warning(format!(
                    "toolchains.xml not found at {}",
                    toolchains_path.display()
                ));
            }
        };

        let home_str = self.jdk_home.to_string_lossy();
        if content.contains(home_str.as_ref()) {
            CompatStatus::Ok
        } else {
            CompatStatus::Warning(
                "toolchains.xml exists but does not reference this JDK home".into(),
            )
        }
    }

    /// Generate a `<toolchain>` XML snippet that can be pasted into
    /// `~/.m2/toolchains.xml`.
    pub fn generate_settings_snippet(&self) -> String {
        let home = self.jdk_home.display();
        format!(
            "\
<toolchain>\n\
  <type>jdk</type>\n\
  <provides>\n\
    <version>25</version>\n\
    <vendor>CratonVM</vendor>\n\
  </provides>\n\
  <configuration>\n\
    <jdkHome>{home}</jdkHome>\n\
  </configuration>\n\
</toolchain>\n"
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// GradleCompatChecker
// ─────────────────────────────────────────────────────────────────────────────

/// Checks and helpers for Gradle compatibility.
#[derive(Debug, Clone)]
pub struct GradleCompatChecker {
    jdk_home: PathBuf,
}

impl GradleCompatChecker {
    pub fn new(jdk_home: impl Into<PathBuf>) -> Self {
        Self {
            jdk_home: jdk_home.into(),
        }
    }

    /// Verify that `jdk_home` looks valid for Gradle's JVM detection.
    ///
    /// Gradle probes `release`, `bin/java`, and optionally `lib/jvm.cfg`.
    pub fn check_java_home_valid(&self) -> CompatStatus {
        if !self.jdk_home.join("release").is_file() {
            return CompatStatus::Error("release file missing; Gradle cannot detect JDK".into());
        }

        let java_exists = if cfg!(unix) {
            self.jdk_home.join("bin/java").exists()
        } else {
            self.jdk_home.join("bin/java.cmd").exists()
        };
        if !java_exists {
            return CompatStatus::Error("bin/java not found".into());
        }

        if !self.jdk_home.join("lib/jvm.cfg").is_file() {
            return CompatStatus::Warning(
                "lib/jvm.cfg missing; some Gradle plugins may warn".into(),
            );
        }

        CompatStatus::Ok
    }

    /// Generate a `gradle.properties` snippet that sets `org.gradle.java.home`.
    pub fn generate_gradle_properties(&self) -> String {
        let home = self.jdk_home.display();
        // Gradle properties use forward slashes even on Windows.
        let normalized = format!("{home}").replace('\\', "/");
        format!("org.gradle.java.home={normalized}\n")
    }

    /// Generate a Kotlin DSL toolchain spec for `build.gradle.kts`.
    pub fn generate_toolchain_spec(&self) -> String {
        "\
java {\n\
    toolchain {\n\
        languageVersion.set(JavaLanguageVersion.of(25))\n\
        vendor.set(JvmVendorSpec.matching(\"CratonVM\"))\n\
    }\n\
}\n"
        .to_string()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Convenience: write a snippet directly to a file
// ─────────────────────────────────────────────────────────────────────────────

/// Write `content` to `path`, creating parent directories as needed.
pub fn write_snippet_to_file(path: &Path, content: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
