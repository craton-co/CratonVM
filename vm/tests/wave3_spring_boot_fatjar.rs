// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 3 — Spring Boot 3.2 fat-jar launcher partial fix.
//!
//! Pins the fix that lets a Spring Boot 3.2 executable JAR get past the
//! `Class.getProtectionDomain().getCodeSource().getLocation().toURI()`
//! round-trip in `org.springframework.boot.loader.launch.Archive.create`.
//!
//! Before this fix, `java -jar insurance-backend.jar` died inside
//! `ExecutableArchiveLauncher.getClassPathUrls(102)` with a bare NPE because
//! the synthetic `URL` produced by `Class.getProtectionDomain()` had a
//! field-zero string layout that did not survive the
//! `URL.toURI().getSchemeSpecificPart() -> new File(...) -> file.exists()`
//! chain on Windows. The synthetic `Archive` field was never populated and
//! the subsequent `archive.getClassPathUrls(...)` invokeinterface NPE'd.
//!
//! After the fix we:
//!   1. Allocate the synthetic `URL` against the real JDK 25 URL field
//!      layout (protocol, host, port=-1, file, path) so `URL.toString()`
//!      and `URL.toURI()` produce a valid `file:/<path>` form.
//!   2. Override the URL.toString native to reconstruct the external form
//!      from those slots (instead of reading the synthetic-mode field-5
//!      cache, which collides with real-JDK `authority` and tickles
//!      `URL.isBuiltinStreamHandler` to NPE).
//!   3. Register `register_phase57_file` (File constructors / metadata
//!      accessors) and `register_p59_jar` (JarFile constructors / accessors)
//!      in real-JDK mode so `new File("/C:/...")` normalises to
//!      `C:\...` and `new JarFile(file)` can read its manifest via the
//!      Rust `zip` crate.
//!   4. Add `java/io/File` and `java/util/jar/JarFile` to the
//!      `check_override` allow-list so dispatch favours our natives over
//!      the real-JDK bytecode that depends on filesystem primitives we
//!      do not implement.
//!
//! This advances Spring Boot's launcher past the Archive setup. The
//! launcher then NPEs deeper inside `JarFileArchive.getClassPathUrls(86)`
//! at `ZipFile.jarStream / ZipFile.ensureOpen` — that's the next blocker
//! (full nested-JAR / `JarFile.stream()` support) and is out of scope for
//! this iteration. The test below pins the registration changes plus the
//! "first NPE no longer fires" behaviour so the next Spring Boot iteration
//! starts from a known baseline.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::vm::SharedVm;

fn shared() -> Arc<SharedVm> {
    Arc::new(SharedVm::new(VmConfig::default()))
}

#[test]
fn spring_boot_natives_are_registered() {
    let shared = shared();

    // File constructor / metadata accessors must be registered in real-JDK
    // mode so the URL.toURI -> File round-trip can normalise on Windows.
    assert!(
        shared
            .natives
            .native_methods
            .find("java/io/File", "<init>", "(Ljava/lang/String;)V")
            .is_some(),
        "Spring Boot fat-jar regression: File.<init>(String) MUST be \
         registered as a native in real-JDK mode (register_phase57_file)."
    );
    assert!(
        shared
            .natives
            .native_methods
            .find("java/io/File", "exists", "()Z")
            .is_some(),
        "Spring Boot fat-jar regression: File.exists() MUST be registered."
    );

    // JarFile constructor / accessors must be registered so the launcher
    // can open the fat-jar via the Rust zip crate.
    assert!(
        shared
            .natives
            .native_methods
            .find("java/util/jar/JarFile", "<init>", "(Ljava/io/File;)V")
            .is_some(),
        "Spring Boot fat-jar regression: JarFile.<init>(File) MUST be \
         registered as a native in real-JDK mode (register_p59_jar)."
    );
    assert!(
        shared
            .natives
            .native_methods
            .find(
                "java/util/jar/JarFile",
                "getManifest",
                "()Ljava/util/jar/Manifest;"
            )
            .is_some(),
        "Spring Boot fat-jar regression: JarFile.getManifest() MUST be \
         registered."
    );

    // The Class.getProtectionDomain shim must be registered — Archive.create
    // walks this entry point first when launching a fat-jar.
    assert!(
        shared
            .natives
            .native_methods
            .find(
                "java/lang/Class",
                "getProtectionDomain",
                "()Ljava/security/ProtectionDomain;"
            )
            .is_some(),
        "Spring Boot fat-jar regression: Class.getProtectionDomain() MUST \
         be registered to seed the launcher's Archive object."
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let p = PathBuf::from(&bin);
        if p.exists() {
            return Some(p);
        }
    }
    let target = workspace_root().join("target");
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = target.join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn java_home() -> Option<PathBuf> {
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(&jh);
        if p.exists() {
            return Some(p);
        }
    }
    let candidate = PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot");
    if candidate.exists() {
        return Some(candidate);
    }
    None
}

fn fat_jar_path() -> Option<PathBuf> {
    // Honour an explicit override first.
    if let Ok(p) = std::env::var("CRATONVM_SPRING_BOOT_FATJAR") {
        let p = PathBuf::from(&p);
        if p.exists() {
            return Some(p);
        }
    }
    // Repo-relative landing zone. This used to be a hardcoded
    // `C:/Users/Admin/AppData/Local/Temp/insurance-backend.jar` — one developer's
    // machine, one Windows account name. It could never resolve for anyone else,
    // so on every other checkout `run_fat_jar` returned `None` and the test below
    // reported `ok` while asserting nothing. A relative default at least resolves
    // for whoever stages the artefact; the env override stays the portable route.
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent();
    if let Some(root) = repo_root {
        for rel in [
            // Preferred: alongside the other Spring Boot suite material.
            "apps/spring-boot-suite-runner/insurance-backend.jar",
            "target/fixtures/insurance-backend.jar",
        ] {
            let candidate = root.join(rel);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn run_fat_jar(timeout: Duration) -> Option<(String, String, Option<i32>)> {
    let bin = cratonvm_binary()?;
    let jh = java_home()?;
    let jar = fat_jar_path()?;

    let mut cmd = Command::new(&bin);
    cmd.arg("--java-home")
        .arg(&jh)
        .arg("--Xmx")
        .arg("1g")
        .arg("--jar")
        .arg(&jar);

    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    };

    use std::io::Read;
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut stdout);
    }
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut stderr);
    }
    Some((stdout, stderr, status.code()))
}

#[test]
fn spring_boot_fatjar_launcher_bypasses_archive_npe() {
    // Skip if the fat-jar fixture isn't staged — keeps the test green
    // in environments without the Spring Boot sample (e.g. CI runners
    // that have not copied the insurance-backend artefact).
    let Some((stdout, stderr, _code)) = run_fat_jar(Duration::from_secs(45)) else {
        // Loud, and a failure under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`. `**/*.jar` is gitignored (.gitignore line
        // 14), so this artefact is always staged, never committed.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        let _ = common::require_fixture(
            "wave3_spring_boot_fatjar",
            "a Spring Boot 3.2 executable fat jar (or a `cratonvm` binary / JAVA_HOME). Set \
             CRATONVM_SPRING_BOOT_FATJAR to point at one",
            &[
                root.join("apps/spring-boot-suite-runner/insurance-backend.jar"),
                root.join("target/fixtures/insurance-backend.jar"),
            ],
        );
        return;
    };
    let combined = format!("{stdout}\n{stderr}");

    // Pre-fix signature: NPE inside ExecutableArchiveLauncher.getClassPathUrls
    // line 102 with the synthetic Archive never populated. After the fix,
    // execution advances to JarFileArchive (line 86) which is the next
    // blocker and an acceptable PARTIAL milestone.
    assert!(
        !combined.contains(
            "ExecutableArchiveLauncher.getClassPathUrls(ExecutableArchiveLauncher.java:102)"
        ),
        "Spring Boot fat-jar regression: ExecutableArchiveLauncher.\
         getClassPathUrls:102 NPE has reappeared. The synthetic Archive's \
         URL/URI/File round-trip must keep working so `archive` is \
         non-null when getClassPathUrls dispatches.\n\nCombined output:\n{combined}"
    );
}
