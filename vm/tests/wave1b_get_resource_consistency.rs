// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-1 Task B — `ClassLoader.getResource` (singular) must agree
//! with `ClassLoader.getResources` (plural enumeration). When the
//! plural enumerator returns N≥1 URLs for a name, the singular
//! lookup MUST return the first of those URLs (not null, not a
//! different URL form).
//!
//! Roadmap reference: `roadmap-any-java-app.md` Wave 1 Task B.
//!
//! Why this is a real-JDK-mode pin (not a `vm.invoke` Rust unit test):
//!   * The bug shape is "the cl_get_resource native override is never
//!     consulted for real-JDK `ClassLoader.getResource` because the
//!     method is concrete; the JDK implementation runs and returns
//!     null because URLClassPath.<clinit> swallows in real-JDK
//!     bootstrap." That symptom only reproduces against a real
//!     Adoptium 25 boot image with `use_synthetic_jdk = false`.
//!   * The complementary `RSLF4J.1` test
//!     (`vm/tests/rslf4j1_get_resources.rs`) covers the bulk path;
//!     this test pins the singular path so the two stay in lock-step.
//!
//! Reproducer parity:
//!   1. Build a synthetic `.jar` containing only an SPI descriptor
//!      (`META-INF/services/wave1b.foo.svc`).
//!   2. Stage a tiny `SingleLookup` class on a separate dir classpath
//!      entry whose `main` enumerates the resource via
//!      `getSystemClassLoader().getResource(...)` and prints
//!      `url=<url>` and `notnull=true|false`.
//!   3. Spawn the VM, parse stdout, assert the URL is non-null.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

const RESOURCE_NAME: &str = "META-INF/services/wave1b.foo.svc";

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
/// descriptor entry, plus a sibling dir holding the compiled
/// `SingleLookup` fixture. Returns `(fixture_dir, jar_path)`.
fn stage_fixture() -> Option<(PathBuf, PathBuf)> {
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    let dir = tempfile::TempDir::new().ok()?;
    let fixture_dir = dir.path().join("fixture");
    std::fs::create_dir_all(&fixture_dir).ok()?;

    let src_path = fixture_dir.join("SingleLookup.java");
    let src = format!(
        r#"import java.util.*;
import java.net.*;
public class SingleLookup {{
    public static void main(String[] args) throws Exception {{
        ClassLoader cl = ClassLoader.getSystemClassLoader();
        URL one = cl.getResource({0:?});
        Enumeration<URL> e = cl.getResources({0:?});
        int n = 0;
        URL first = null;
        while (e.hasMoreElements()) {{
            URL u = e.nextElement();
            if (first == null) first = u;
            n++;
            if (n > 20) break;
        }}
        System.out.println("plural_count=" + n);
        System.out.println("plural_first=" + first);
        System.out.println("singular=" + one);
        System.out.println("notnull=" + (one != null));
        System.out.println("OK");
    }}
}}
"#,
        RESOURCE_NAME
    );
    std::fs::write(&src_path, src).ok()?;

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
        "[wave1b_get_resource_consistency] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let jar_path = dir.path().join("wave1b-resources.jar");
    let f = std::fs::File::create(&jar_path).ok()?;
    let mut zip = ZipWriter::new(f);
    let opts: SimpleFileOptions =
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file(RESOURCE_NAME, opts).ok()?;
    zip.write_all(b"wave1b.foo.svc.DummyProvider\n").ok()?;
    zip.finish().ok()?;

    std::mem::forget(dir);
    Some((fixture_dir, jar_path))
}

/// Join classpath entries with the HOST's path separator — `;` on Windows, `:`
/// everywhere else. See the note on the same function in
/// `rslf4j1_get_resources.rs`: a hard-coded `;` collapses the whole classpath
/// into one entry on Linux and the fixture class is then never found.
fn format_classpath(parts: &[&Path]) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    parts
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(sep)
}

/// Wave-1 Task B acceptance: spawn cratonvm in real-JDK mode against a
/// JAR containing only `META-INF/services/...`, and assert the
/// singular `getResource` returns a non-null URL whenever the plural
/// `getResources` returns ≥ 1.
#[test]
fn singular_get_resource_matches_bulk_get_resources_for_jar_entry() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "Skipping: cratonvm release binary not available at \
                 target/release/cratonvm[.exe]"
            );
            return;
        }
    };
    let java_home = match real_java_home() {
        Some(h) => h,
        None => {
            eprintln!("Skipping: real JDK 25 not available");
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
        .args(["--java-home", &java_home, "-c", &cp, "SingleLookup"])
        .output()
        .expect("must spawn cratonvm");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "cratonvm exited non-zero. stdout: {stdout}\nstderr: {stderr}"
    );

    let plural_count: i32 = stdout
        .lines()
        .find_map(|l| l.strip_prefix("plural_count=").and_then(|n| n.parse().ok()))
        .unwrap_or(-1);
    assert!(
        plural_count >= 1,
        "Precondition: plural getResources({RESOURCE_NAME:?}) returned \
         {plural_count}; expected ≥ 1 from the synthesised JAR.\n\
         JAR: {}\nstdout: {stdout}\nstderr: {stderr}",
        jar_path.display()
    );

    let notnull = stdout.lines().any(|l| l == "notnull=true");
    assert!(
        notnull,
        "ClassLoader.getResource({RESOURCE_NAME:?}) returned null while \
         getResources returned {plural_count} URLs. The singular and \
         bulk paths are inconsistent — wire \
         classloader::cl_get_resource_essential into \
         register_essential_natives so both go through \
         find_all_resource_urls.\n\
         stdout: {stdout}\nstderr: {stderr}"
    );

    // Stronger: the singular URL.toString must equal plural_first.
    let plural_first = stdout
        .lines()
        .find_map(|l| l.strip_prefix("plural_first="))
        .unwrap_or("");
    let singular = stdout
        .lines()
        .find_map(|l| l.strip_prefix("singular="))
        .unwrap_or("");
    assert_eq!(
        plural_first, singular,
        "singular URL must equal plural's first URL. \
         plural_first={plural_first:?} singular={singular:?}\n\
         stdout: {stdout}"
    );
}
