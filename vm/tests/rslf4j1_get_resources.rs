// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RSLF4J.1 — `ClassLoader.getSystemClassLoader().getResources(...)`
//! must enumerate matches that live inside JAR classpath entries when
//! the VM runs in real-JDK mode.
//!
//! Roadmap reference: `roadmap-any-java-app.md` §RSLF4J.1.
//!
//! Why this is a real-JDK-mode pin (not a `vm.invoke` Rust unit test):
//!   * The bug is "the cl_get_resources native override is never
//!     consulted for the *real-JDK* `ClassLoader.getResources` because
//!     the method is concrete." That symptom only reproduces when the
//!     VM runs against a real Adoptium 25 (or other real JDK 25), with
//!     `use_synthetic_jdk = false` and `--java-home` pointing at it —
//!     synthetic-JDK mode has its own getResources stubs and a
//!     `java/util/Enumeration$Impl` shape that diverges from the real
//!     JDK's `Enumeration<URL>` interface dispatch.
//!   * Therefore the test spawns the freshly-built `cratonvm` binary as a
//!     subprocess. Skips when neither the binary nor a JDK 25
//!     `java-home` is available.
//!
//! Reproducer parity:
//!   1. Build a synthetic `.jar` (via the `zip` crate, no `jar` on PATH
//!      required) containing **only** an SPI descriptor entry —
//!      `META-INF/services/cratonvm.foo.svc`.
//!   2. Stage a tiny `EnumLookup` class on a separate dir classpath
//!      entry whose `main` enumerates the resource via
//!      `getSystemClassLoader().getResources(...)` and prints
//!      `total=N\n` lines plus each URL's toString().
//!   3. Spawn the VM, parse stdout, assert ≥ 1 URL with a non-empty
//!      `toString()`.
//!
//! Equivalent to the manual reproducer:
//! ```sh
//! target/release/cratonvm.exe --java-home "C:/Program Files/Eclipse \
//!     Adoptium/jdk-25.0.2.10-hotspot" \
//!     -c "<dir>;<jar>" EnumLookup
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const RESOURCE_NAME: &str = "META-INF/services/cratonvm.foo.svc";

/// Path to the freshly-built CLI binary the harness should exercise.
/// Honors `CRATONVM_BIN` for callers that want to point at a custom
/// build; otherwise resolves to the workspace's
/// `target/release/cratonvm{.exe}`.
mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CRATONVM_BIN") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    // vm/Cargo.toml lives at <repo>/vm; binary lands at
    // <repo>/target/release/cratonvm{.exe}. Probing ONLY `cratonvm.exe` made
    // this fallback unsatisfiable off-Windows, so on Linux the test skipped
    // (and still printed `ok`) unless `CRATONVM_BIN` was set -- which is what
    // kept the hard-coded `;` above invisible for as long as it was.
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    let candidate = PathBuf::from(manifest_dir)
        .parent()
        .map(|p| p.join("target").join("release").join(exe))?;
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

/// Resolve the real-JDK 25 java-home — env var first, then the standard
/// Adoptium install path. Returns None when none is reachable so the
/// test can skip cleanly on machines without the JDK 25 dependency.
fn real_java_home() -> Option<String> {
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        if Path::new(&jh).join("bin/java.exe").exists() || Path::new(&jh).join("bin/java").exists()
        {
            return Some(jh);
        }
    }
    let adoptium = "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot";
    if Path::new(adoptium).join("bin/java.exe").exists() {
        return Some(adoptium.to_string());
    }
    None
}

/// Materialize a temp dir holding a JAR with **only** the SPI
/// descriptor entry, plus a sibling dir holding the compiled `EnumLookup`
/// fixture. Returns `(fixture_dir, jar_path)`. The tempdir is leaked so
/// both survive until process exit.
fn stage_fixture() -> Option<(PathBuf, PathBuf)> {
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    let dir = tempfile::TempDir::new().ok()?;
    let fixture_dir = dir.path().join("fixture");
    std::fs::create_dir_all(&fixture_dir).ok()?;

    // Emit the EnumLookup source so the test's javac compile step
    // produces the .class file inline.  Keeps the fixture trivial and
    // self-contained — the real EnumTest fixture under `apps/enumtest`
    // is not staged in CI.
    let src_path = fixture_dir.join("EnumLookup.java");
    let src = format!(
        r#"import java.util.*;
import java.net.*;
public class EnumLookup {{
    public static void main(String[] args) throws Exception {{
        Enumeration<URL> e =
            ClassLoader.getSystemClassLoader().getResources({:?});
        int count = 0;
        while (e.hasMoreElements()) {{
            URL u = e.nextElement();
            System.out.println("url: " + u);
            count++;
            if (count > 20) break;
        }}
        System.out.println("total=" + count);
        System.out.println("OK");
    }}
}}
"#,
        RESOURCE_NAME
    );
    std::fs::write(&src_path, src).ok()?;

    // Compile via the same `javac` the build script uses. Skip the test
    // when javac isn't on PATH instead of failing CI on an unrelated
    // tooling gap.
    let out = Command::new("javac")
        .args([
            "-d",
            fixture_dir.to_str().unwrap(),
            src_path.to_str().unwrap(),
        ])
        .output()
        .ok()?;
    // javac RAN and rejected the source: the probe is broken, and returning
    // None here reads to the caller as "javac unavailable, skip", which makes
    // this test a permanent vacuous pass.
    assert!(
        out.status.success(),
        "[rslf4j1_get_resources] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let jar_path = dir.path().join("rslf4j1-resources.jar");
    let f = std::fs::File::create(&jar_path).ok()?;
    let mut zip = ZipWriter::new(f);
    let opts: SimpleFileOptions =
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file(RESOURCE_NAME, opts).ok()?;
    zip.write_all(b"cratonvm.foo.svc.DummyProvider\n").ok()?;
    zip.finish().ok()?;

    std::mem::forget(dir);
    Some((fixture_dir, jar_path))
}

/// Format the classpath the way the cratonvm CLI's `-c` flag expects: absolute
/// paths joined by the HOST's path separator — `;` on Windows, `:` everywhere
/// else. Hard-coding `;` made this test Windows-only in fact while reading as
/// portable: measured on Linux, `-c "/dir;/dir/e.jar"` yields
/// `class not found: EnumLookup` (the whole string is taken as ONE entry) where
/// `-c "/dir:/dir/e.jar"` runs. Same idiom as `differential.rs` and
/// `wave1_b2_bootloader_resources.rs`.
fn format_classpath(parts: &[&Path]) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    parts
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(sep)
}

/// RSLF4J.1 acceptance: spawn the freshly-built cratonvm in real-JDK
/// mode, point it at a JAR containing only `META-INF/services/...`,
/// and assert `getSystemClassLoader().getResources(...)` returns at
/// least one URL.
#[test]
fn system_classloader_get_resources_walks_jar_in_real_jdk_mode() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "Skipping: cratonvm release binary not available at \
                 target/release/cratonvm[.exe] (build with `cargo build \
                 --release -p cratonvm-cli`)"
            );
            return;
        }
    };
    let java_home = match real_java_home() {
        Some(h) => h,
        None => {
            eprintln!(
                "Skipping: real JDK 25 not available (set JAVA_HOME or \
                 install Adoptium 25 at the documented path)"
            );
            return;
        }
    };
    let (fixture_dir, jar_path) = match stage_fixture() {
        Some(t) => t,
        None => {
            eprintln!("Skipping: could not stage JAR fixture (javac not on PATH?)");
            return;
        }
    };
    let cp = format_classpath(&[&fixture_dir, &jar_path]);
    let output = Command::new(&bin)
        .args(["--java-home", &java_home, "-c", &cp, "EnumLookup"])
        .output()
        .expect("must spawn cratonvm");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "cratonvm exited non-zero. stdout: {stdout}\nstderr: {stderr}"
    );

    // Parse `total=N` from stdout — the fixture prints it on its own line.
    let total: i32 = stdout
        .lines()
        .find_map(|l| l.strip_prefix("total=").and_then(|n| n.parse().ok()))
        .unwrap_or(-1);
    assert!(
        total >= 1,
        "ClassLoader.getSystemClassLoader().getResources({RESOURCE_NAME:?}) \
         returned {total} entries (expected >= 1). The cl_get_resources \
         native override is either not consulted (vm_exec.rs allowlist), \
         not registered for real-JDK mode, or find_all_resource_urls is \
         not walking JAR classpath entries.\n\
         JAR was at: {}\nstdout: {stdout}\nstderr: {stderr}",
        jar_path.display()
    );

    // Extra robustness: at least one printed URL must round-trip a
    // non-empty toString.  Catches a regression where the count is
    // non-zero but the URL synthesis path fabricates null fields.
    let url_lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("url: ")).collect();
    assert!(
        !url_lines.is_empty(),
        "expected at least one `url: ...` line, got stdout: {stdout}"
    );
    assert!(
        url_lines.iter().any(|l| l.contains(RESOURCE_NAME)),
        "expected at least one URL whose toString contains \
         {RESOURCE_NAME:?}, got: {url_lines:?}"
    );
}

/// Anchor: the synthesised JAR really contains the SPI descriptor.
/// Catches a regression where the fixture scaffolding renames the
/// entry and silently no-ops the JAR-side path.
#[test]
fn synthesised_jar_contains_resource() {
    let (_dir, jar) = match stage_fixture() {
        Some(t) => t,
        None => {
            eprintln!("Skipping: could not stage JAR fixture (javac not on PATH?)");
            return;
        }
    };
    let bytes = std::fs::read(&jar).expect("read synthesised jar");
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).expect("open jar as zip");
    let mut entry = archive
        .by_name(RESOURCE_NAME)
        .unwrap_or_else(|_| panic!("{RESOURCE_NAME} entry present in jar"));
    use std::io::Read;
    let mut content = String::new();
    entry.read_to_string(&mut content).expect("read entry");
    assert!(
        content.contains("DummyProvider"),
        "SPI descriptor missing expected provider FQN, got: {content:?}",
    );
}
