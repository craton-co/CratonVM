// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A `JarFile` subclass resolves `super.close()` through its immediate
//! `JarFile` constant-pool reference, but the inherited declaration belongs
//! to `ZipFile`.  CratonVM creates the jar through its native bridge, so the
//! real `ZipFile.close()` body would dereference its uninitialised `res`
//! field.  This must therefore reach the registered ZipFile bridge in both
//! execution modes.

use std::path::{Path, PathBuf};
use std::process::Command;

const CLASS_NAME: &str = "ZipFileInheritedSuperNativeBridgeProbe";

const SOURCE: &str = r#"
import java.io.File;
import java.util.jar.JarFile;

public final class ZipFileInheritedSuperNativeBridgeProbe extends JarFile {
    private ZipFileInheritedSuperNativeBridgeProbe(File file) throws Exception {
        super(file);
    }

    private void closeThroughJarFileSuper() throws Exception {
        super.close();
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            throw new AssertionError("missing archive path");
        }
        ZipFileInheritedSuperNativeBridgeProbe jar =
            new ZipFileInheritedSuperNativeBridgeProbe(new File(args[0]));
        jar.closeThroughJarFileSuper();
        // Exercise the warmed invokespecial cache as well as the first
        // resolution. The native close bridge is intentionally idempotent.
        jar.closeThroughJarFileSuper();
        System.out.println("ZIPFILE_INHERITED_SUPER_NATIVE_BRIDGE_PASS");
    }
}
"#;

fn java_home() -> Option<PathBuf> {
    ["CRATONVM_TEST_JAVA_HOME", "CRATONVM_JAVA_HOME", "JAVA_HOME"]
        .into_iter()
        .filter_map(|variable| std::env::var(variable).ok())
        .map(PathBuf::from)
        .find(|path| {
            path.join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" })
                .exists()
        })
        .or_else(|| {
            [
                "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot",
                "C:/Program Files/Java/jdk-25",
                "/home/victor/jdk25",
                "/data/data/jdk25-real",
            ]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| {
                path.join("bin")
                    .join(if cfg!(windows) { "javac.exe" } else { "javac" })
                    .exists()
            })
        })
}

fn compile_probe(java_home: &Path, classes: &Path) {
    std::fs::create_dir_all(classes).expect("create probe directory");
    let source = classes.join(format!("{CLASS_NAME}.java"));
    std::fs::write(&source, SOURCE).expect("write probe source");
    let status =
        Command::new(
            java_home
                .join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" }),
        )
        .args(["--release", "21", "-d"])
        .arg(classes)
        .arg(&source)
        .output()
        .expect("run javac");
    // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
    // means this javac is older than the level this probe compiles at, so it never
    // opened the file. That is a missing-toolchain condition — the same one the
    // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
    // source" sends the next reader to edit a correct `.java` file.
    //
    // Narrowly keyed on javac's own wording for an unsupported release, so a
    // genuine source error still reaches the assertion below and still fails loudly
    // (see `probe_compile_guard.rs` for why that must never become a skip).
    if !status.status.success() {
        let stderr_probe = String::from_utf8_lossy(&status.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[zipfile_inherited_super_native_bridge] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return;
        }
    }
    assert!(
        status.status.success(),
        "[zipfile_inherited_super_native_bridge] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
}

fn write_empty_jar(path: &Path) {
    // An empty ZIP archive has just an EOCD record.  Supplying it from Rust
    // keeps the probe focused on the JarFile dispatch path.
    std::fs::write(
        path,
        [
            0x50, 0x4b, 0x05, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ],
    )
    .expect("write empty jar");
}

#[test]
fn zipfile_inherited_super_close_uses_native_bridge_in_jit_and_nojit() {
    let Some(java_home) = java_home() else {
        eprintln!("[zipfile-inherited-super] JDK unavailable; skipping");
        return;
    };
    let Some(binary) = std::env::var("CRATONVM_BIN").ok().map(PathBuf::from) else {
        eprintln!("[zipfile-inherited-super] CRATONVM_BIN is required; skipping");
        return;
    };
    assert!(
        binary.exists(),
        "CratonVM binary missing: {}",
        binary.display()
    );

    let dir = std::env::temp_dir().join(format!(
        "cratonvm-zipfile-inherited-super-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let classes = dir.join("classes");
    compile_probe(&java_home, &classes);
    let jar = dir.join("empty.jar");
    write_empty_jar(&jar);

    for nojit in [false, true] {
        let mut command = Command::new(&binary);
        command.arg("--java-home").arg(&java_home);
        if nojit {
            command.arg("--nojit");
        }
        let output = command
            .arg("-cp")
            .arg(&classes)
            .arg(CLASS_NAME)
            .arg(&jar)
            .output()
            .expect("run CratonVM probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "nojit={nojit} stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            stdout.contains("ZIPFILE_INHERITED_SUPER_NATIVE_BRIDGE_PASS"),
            "nojit={nojit} stdout:\n{stdout}"
        );
    }
}
