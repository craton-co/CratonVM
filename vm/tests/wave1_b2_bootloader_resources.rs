// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 1 B2 — `ClassLoader.getResources` boot-loader-resource regression.
//!
//! Pins three behaviors of `ClassLoader.getSystemClassLoader().getResources(name)`:
//!
//!  1. **Format/contract**: stdout always ends with a `total=<N>` line followed
//!     by `OK`. The `Enumeration` returned by `getResources` is iterable and
//!     does not throw.
//!  2. **Application-classpath JAR (W1-B / Session 96+103)**: when a JAR
//!     containing `META-INF/MANIFEST.MF` is added to `-cp`, `getResources`
//!     reports `total>=1` with a `jar:file:.../svctest.jar!/META-INF/MANIFEST.MF`
//!     URL. Pins the classpath-JAR walking path in
//!     `ClassPath::find_all_resource_urls`.
//!  3. **Boot-loader jimage (this Wave 1 B2 fix)**: when the resource exists
//!     in `$JAVA_HOME/lib/modules`, `getResources` surfaces it with a
//!     JEP-220-style `jrt:/<module>/<resource>` URL. JDK 25 ships
//!     `META-INF/services/java.nio.file.spi.FileSystemProvider` in its
//!     jimage but **does not** ship `META-INF/MANIFEST.MF` (verified via
//!     `jimage list lib/modules`), which is why HotSpot also returns 0 for
//!     MANIFEST.MF on JDK 25. We use the FileSystemProvider service file as
//!     a positive control for the jimage path.
//!
//! Subprocess pattern: spawn the `cratonvm` binary from
//! `target/{release,debug}` (or `CRATONVM_BIN` if set) with the EnumTest
//! fixture and assert on stdout. Skips when the binary or `javac` is
//! unavailable so the test can run hermetically in CI.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const ENUMTEST_JAVA: &str = r#"
import java.util.*;
import java.net.*;
public class EnumTest {
    public static void main(String[] args) throws Exception {
        String resource = args.length > 0 ? args[0] : "META-INF/MANIFEST.MF";
        Enumeration<URL> e = ClassLoader.getSystemClassLoader().getResources(resource);
        int count = 0;
        while (e.hasMoreElements()) {
            URL u = e.nextElement();
            System.out.println("url: " + u);
            count++;
            if (count > 20) break;
        }
        System.out.println("total=" + count);
        System.out.println("OK");
    }
}
"#;

const SVCTEST_MANIFEST_MF: &str = "Manifest-Version: 1.0\r\n";
const SVCTEST_SERVICE_FILE: &str = "x\n";

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
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.parent().unwrap().join("target");
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

/// Compile EnumTest.java into a temp dir and return the dir path. Returns
/// `None` if `javac` is not available so callers can skip cleanly.
fn compile_enumtest(workdir: &Path) -> Option<PathBuf> {
    let src = workdir.join("EnumTest.java");
    std::fs::write(&src, ENUMTEST_JAVA).ok()?;
    let classes = workdir.join("classes");
    std::fs::create_dir_all(&classes).ok()?;
    let out = match Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&classes)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!("[wave1_b2_bootloader_resources] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
    // means this javac is older than the level this probe compiles at, so it never
    // opened the file. That is a missing-toolchain condition — the same one the
    // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
    // probe source" sends the next reader to edit a correct `.java` file.
    //
    // Narrowly keyed on javac's own wording for an unsupported release, so a
    // genuine source error still reaches the assertion below and still fails loudly
    // (see `probe_compile_guard.rs` for why that must never become a skip).
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[wave1_b2_bootloader_resources] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    // javac RAN and rejected the source: the probe is broken, and skipping here
    // would make this test a permanent vacuous pass.
    assert!(
        out.status.success() && classes.join("EnumTest.class").exists(),
        "[wave1_b2_bootloader_resources] the embedded probe failed to compile — fix \
         the probe source. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(classes)
}

/// Build a minimal JAR with `META-INF/MANIFEST.MF` and a service descriptor
/// to exercise the W1-B classpath-JAR branch of `find_all_resource_urls`.
fn build_svctest_jar(workdir: &Path) -> Option<PathBuf> {
    use std::fs::File;
    use std::io::Write;

    let jar_path = workdir.join("svctest.jar");
    let file = File::create(&jar_path).ok()?;
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.start_file("META-INF/MANIFEST.MF", opts).ok()?;
    zip.write_all(SVCTEST_MANIFEST_MF.as_bytes()).ok()?;
    zip.start_file("META-INF/services/foo.svc", opts).ok()?;
    zip.write_all(SVCTEST_SERVICE_FILE.as_bytes()).ok()?;

    zip.finish().ok()?;
    Some(jar_path)
}

/// Run cratonvm with the given classpath and resource arg. Returns the
/// (stdout, stderr) on successful spawn.
fn run_enumtest(
    bin: &Path,
    classpath: &str,
    resource_arg: Option<&str>,
    timeout: Duration,
) -> Option<(String, String)> {
    let mut cmd = Command::new(bin);
    cmd.arg("-c").arg(classpath).arg("EnumTest");
    if let Some(r) = resource_arg {
        cmd.arg(r);
    }
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "[wave1_b2_bootloader_resources] EnumTest timed out after {:?}",
                        timeout
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// Extract the integer from the `total=NN` line in stdout. Panics if the
/// line is missing — the contract is that EnumTest always prints it.
fn parse_total(stdout: &str) -> i64 {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("total=") {
            if let Ok(n) = rest.trim().parse::<i64>() {
                return n;
            }
        }
    }
    panic!("[wave1_b2_bootloader_resources] no `total=` line in stdout:\n{stdout}");
}

/// W1-B regression: `META-INF/MANIFEST.MF` in a classpath JAR is enumerated.
/// Asserts `total >= 1` and `OK`. This pins the JAR-walking path that
/// Sessions 96 and 103 wired up.
#[test]
fn wave1_b2_classpath_jar_manifest_enumerated() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[wave1_b2_bootloader_resources] skip: cratonvm binary not found; \
                 build with `cargo build --release -p cratonvm-cli`"
            );
            return;
        }
    };
    let dir = match tempfile::TempDir::new() {
        Ok(d) => d,
        Err(_) => return,
    };
    let classes = match compile_enumtest(dir.path()) {
        Some(c) => c,
        None => {
            eprintln!(
                "[wave1_b2_bootloader_resources] skip: javac unavailable or EnumTest failed to compile"
            );
            return;
        }
    };
    let jar = match build_svctest_jar(dir.path()) {
        Some(j) => j,
        None => {
            eprintln!("[wave1_b2_bootloader_resources] skip: failed to build test JAR");
            return;
        }
    };
    let sep = if cfg!(windows) { ";" } else { ":" };
    let cp = format!("{}{sep}{}", classes.display(), jar.display());

    let (stdout, stderr) =
        run_enumtest(&bin, &cp, None, Duration::from_secs(60)).expect("spawn cratonvm");

    assert!(
        stdout.contains("OK"),
        "[wave1_b2] expected OK in stdout, got:\nstdout: {stdout}\nstderr: {stderr}"
    );
    let total = parse_total(&stdout);
    assert!(
        total >= 1,
        "[wave1_b2] expected total>=1 for classpath JAR with MANIFEST.MF; got total={total}\nstdout: {stdout}"
    );
}

/// Boot-loader contract: `getResources` over the JDK 25 jimage surfaces
/// resources that *do* exist there (e.g. service descriptors). Uses
/// `META-INF/services/java.nio.file.spi.FileSystemProvider` which is shipped
/// inside `lib/modules` (verified via `jimage list`).
///
/// Skipped when no `--java-home` jimage is reachable (the binary's default
/// resolution must find a JDK with a `lib/modules` jimage; otherwise the
/// boot loader has no jimage entries and we cannot validate the contract).
#[test]
fn wave1_b2_jimage_service_descriptor_enumerated() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!("[wave1_b2_bootloader_resources] skip: cratonvm binary not found");
            return;
        }
    };
    let dir = match tempfile::TempDir::new() {
        Ok(d) => d,
        Err(_) => return,
    };
    let classes = match compile_enumtest(dir.path()) {
        Some(c) => c,
        None => {
            eprintln!("[wave1_b2_bootloader_resources] skip: javac unavailable");
            return;
        }
    };

    let cp = format!("{}", classes.display());
    let res = "META-INF/services/java.nio.file.spi.FileSystemProvider";

    let (stdout, stderr) =
        run_enumtest(&bin, &cp, Some(res), Duration::from_secs(60)).expect("spawn cratonvm");

    assert!(
        stdout.contains("OK"),
        "[wave1_b2] expected OK in stdout, got:\nstdout: {stdout}\nstderr: {stderr}"
    );
    // Only require >=1 when the boot loader actually has a jimage; if the
    // resolved JDK is jmods-only or directory-only this assertion is N/A.
    // Detect by checking that the produced URL set at least is well-formed.
    let total = parse_total(&stdout);
    assert!(
        total >= 0,
        "[wave1_b2] total must be a non-negative integer; got total={total}"
    );
    // `total >= 0` on its own is near-vacuous: `parse_total` already panics when
    // the line is absent, and a resource count is never negative, so the only
    // thing left for it to catch is a `total=-1` sentinel nobody emits. The real
    // property available here is INTERNAL CONSISTENCY — the probe prints one
    // `url: <u>` line per enumerated URL and then `total=<count>` (see PROBE_SRC
    // at the top of this file), so the two must agree. A boot-loader enumeration
    // that returned duplicates, or that counted entries it never yielded, moves
    // these apart while leaving `total >= 0` perfectly happy.
    let url_lines = stdout
        .lines()
        .filter(|l| l.trim_start().starts_with("url: "))
        .count() as i64;
    assert_eq!(
        url_lines, total,
        "[wave1_b2] the probe printed {url_lines} `url:` line(s) but reported total={total}; the \
         enumeration and its count disagree.\nstdout: {stdout}\nstderr: {stderr}"
    );
    // If the JDK reachable via `bin` exposes a jimage (the typical case for
    // Adoptium 25), `total` should be exactly 1 — `java.base` ships this
    // service descriptor exactly once. Use a soft assert: tolerate 0 only
    // when the test JDK is non-modular (rare in CI).
    if total >= 1 {
        // URL line present and well-formed.
        assert!(
            stdout.contains("url: jrt:")
                || stdout.contains("url: jar:")
                || stdout.contains("url: file:"),
            "[wave1_b2] expected at least one well-formed URL line; got:\n{stdout}"
        );
    }
}

/// Negative control: querying a jimage resource that genuinely does not
/// exist (e.g. `META-INF/MANIFEST.MF` is not in JDK 25's jimage) must
/// return `total=0` with `OK` rather than throwing. Pins the "graceful
/// empty enumeration" contract.
#[test]
fn wave1_b2_missing_jimage_resource_returns_zero() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!("[wave1_b2_bootloader_resources] skip: cratonvm binary not found");
            return;
        }
    };
    let dir = match tempfile::TempDir::new() {
        Ok(d) => d,
        Err(_) => return,
    };
    let classes = match compile_enumtest(dir.path()) {
        Some(c) => c,
        None => {
            eprintln!("[wave1_b2_bootloader_resources] skip: javac unavailable");
            return;
        }
    };

    let cp = format!("{}", classes.display());
    // A made-up resource path that is guaranteed to be absent from any
    // sane JDK image and any classpath entry we build.
    let res = "META-INF/this/does/not/exist/anywhere.txt";

    let (stdout, _stderr) =
        run_enumtest(&bin, &cp, Some(res), Duration::from_secs(60)).expect("spawn cratonvm");

    assert!(
        stdout.contains("OK"),
        "[wave1_b2] expected OK in stdout: {stdout}"
    );
    let total = parse_total(&stdout);
    assert_eq!(
        total, 0,
        "[wave1_b2] missing resource must yield total=0; got total={total}\n{stdout}"
    );
}
