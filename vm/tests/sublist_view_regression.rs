// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression pins for two general correctness bugs in the
//! `cratonvm/internal/ArrayListSubList` view (the backing class for
//! `java.util.ArrayList.subList(...)`).
//!
//! Both are app-independent and live in `native-collections/src/lib.rs`
//! (the `ASL_CLASS = "cratonvm/internal/ArrayListSubList"` block):
//!
//!   * HOLE #1 — missing `toArray(T[])` / `toArray(IntFunction)` overloads.
//!     Only the no-arg `toArray()` was registered, so any
//!     `subList(..).toArray(new X[0])` / `subList(..).toArray(X[]::new)`
//!     hit `NoSuchMethodError` on the synthetic ASL class. The JUnit
//!     Platform launcher itself calls `toArray(T[])`, so this broke every
//!     test class run via the launcher.
//!
//!   * HOLE #2 — `new ArrayList<>(x.subList(..))` / `addAll(subList)` →
//!     SIGSEGV. `collect_collection_elements` (the generic collection→Vec
//!     collector behind collection-copy constructors and addAll) didn't
//!     recognize the ASL layout `(parent, offset, size, expected)` and
//!     misread it through the generic 2-field probes as `[null, <garbage>]`.
//!     The garbage ref was later dispatched (e.g. `StringBuilder.append`),
//!     causing a wild call → `EXCEPTION_ACCESS_VIOLATION`. Reproduces under
//!     `--nojit`, so it is not a JIT bug.
//!
//! Strategy: a self-contained probe is compiled inline (no reliance on the
//! gitignored `apps/` tree), then run under HotSpot and under CratonVM
//! (both default and `--nojit`). CratonVM stdout must match HotSpot
//! byte-for-byte and the process must exit cleanly (no SIGSEGV).

use std::path::{Path, PathBuf};
use std::process::Command;

const PROBE_SRC: &str = r#"import java.util.*;
public class SubListProbe {
    public static void main(String[] args) {
        List<String> base = new ArrayList<>(List.of("a", "b", "c", "d", "e"));
        List<String> sub = base.subList(0, 3);

        // HOLE #1a: toArray(T[]) overload.
        String[] arr = sub.toArray(new String[0]);
        System.out.println("toArrayTyped.len=" + arr.length);
        System.out.println("toArrayTyped=" + Arrays.toString(arr));

        // HOLE #1b: toArray(IntFunction) overload.
        String[] arr2 = sub.toArray(String[]::new);
        System.out.println("toArrayGen.len=" + arr2.length);
        System.out.println("toArrayGen=" + Arrays.toString(arr2));

        // HOLE #2a: copy-construct from a subList view, then iterate and feed
        // each element to StringBuilder.append(Object) (the wild-call site).
        List<String> copy = new ArrayList<>(sub);
        System.out.println("copy.size=" + copy.size());
        StringBuilder sb = new StringBuilder();
        for (String s : copy) sb.append(s).append(',');
        System.out.println("copy.iter=" + sb);
        boolean identity = true;
        for (int i = 0; i < 3; i++) identity &= (copy.get(i) == base.get(i));
        System.out.println("copy.identity=" + identity);

        // HOLE #2b: addAll(subList).
        List<String> target = new ArrayList<>();
        target.add("z");
        target.addAll(sub);
        System.out.println("addAll.size=" + target.size());
        System.out.println("addAll=" + target);

        System.out.println("OK");
    }
}
"#;

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
    let target = PathBuf::from(manifest_dir).parent()?.join("target");
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

fn real_java_home() -> Option<String> {
    for var in &["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(jh) = std::env::var(var) {
            if Path::new(&jh).join("bin/java.exe").exists()
                || Path::new(&jh).join("bin/java").exists()
            {
                return Some(jh);
            }
        }
    }
    let adoptium = "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot";
    if Path::new(adoptium).join("bin/java.exe").exists() {
        return Some(adoptium.to_string());
    }
    None
}

/// Compile the probe into a fresh temp dir. Leaked so the .class survives
/// until process exit. Returns the classes dir, or None if javac is absent.
fn stage_probe() -> Option<PathBuf> {
    let dir = tempfile::TempDir::new().ok()?;
    let classes = dir.path().to_path_buf();
    let src_path = classes.join("SubListProbe.java");
    std::fs::write(&src_path, PROBE_SRC).ok()?;
    let out = Command::new("javac")
        .args([
            "--release",
            "21",
            "-d",
            classes.to_str().unwrap(),
            src_path.to_str().unwrap(),
        ])
        .output()
        .ok()?;
    // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
    // means this javac is older than the level this probe compiles at, so it never
    // opened the file. That is a missing-toolchain condition — the same one the
    // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
    // source" sends the next reader to edit a correct `.java` file.
    //
    // Narrowly keyed on javac's own wording for an unsupported release, so a
    // genuine source error still reaches the assertion below and still fails loudly
    // (see `probe_compile_guard.rs` for why that must never become a skip).
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[sublist_view_regression] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    // javac RAN and rejected the source: the probe is broken, and returning
    // None here reads to the caller as "javac unavailable, skip", which makes
    // this test a permanent vacuous pass.
    assert!(
        out.status.success(),
        "[sublist_view_regression] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::mem::forget(dir);
    Some(classes)
}

fn run_hotspot(java_home: &str, classes: &Path) -> String {
    let java =
        Path::new(java_home)
            .join("bin")
            .join(if cfg!(windows) { "java.exe" } else { "java" });
    let out = Command::new(java)
        .args(["-cp", classes.to_str().unwrap(), "SubListProbe"])
        .output()
        .expect("must spawn HotSpot java");
    assert!(
        out.status.success(),
        "HotSpot exited non-zero. stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
}

/// Run the probe under CratonVM and assert clean exit + stdout == HotSpot.
fn run_cratonvm_and_assert(
    bin: &Path,
    java_home: &str,
    classes: &Path,
    expected: &str,
    extra: &[&str],
) {
    let mut cmd = Command::new(bin);
    cmd.args(["--java-home", java_home, "-c", classes.to_str().unwrap()]);
    for a in extra {
        cmd.arg(a);
    }
    cmd.arg("SubListProbe");
    let out = cmd.output().expect("must spawn cratonvm");
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mode = if extra.is_empty() {
        "default"
    } else {
        "--nojit"
    };
    assert!(
        out.status.success(),
        "[{mode}] cratonvm exited non-zero (SIGSEGV regression?). \
         status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );
    assert!(
        stdout.contains("OK"),
        "[{mode}] probe did not reach OK.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        stdout, expected,
        "[{mode}] CratonVM stdout diverged from HotSpot.\nstderr:\n{stderr}"
    );
}

#[test]
fn sublist_view_toarray_and_copy_match_hotspot() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!("Skipping: cratonvm binary not found (build -p cratonvm-cli)");
            return;
        }
    };
    let java_home = match real_java_home() {
        Some(h) => h,
        None => {
            eprintln!("Skipping: real JDK not found (set JAVA_HOME / CRATONVM_TEST_JDK)");
            return;
        }
    };
    let classes = match stage_probe() {
        Some(c) => c,
        None => {
            eprintln!("Skipping: could not compile probe (javac on PATH?)");
            return;
        }
    };

    let expected = run_hotspot(&java_home, &classes);

    // Sanity: HotSpot itself must produce the shape we expect, so a future
    // probe edit can't silently make both sides agree on garbage.
    assert!(
        expected.contains("copy.size=3"),
        "HotSpot baseline wrong:\n{expected}"
    );
    assert!(
        expected.contains("copy.identity=true"),
        "HotSpot baseline wrong:\n{expected}"
    );
    assert!(
        expected.contains("toArrayTyped.len=3"),
        "HotSpot baseline wrong:\n{expected}"
    );

    // Default (JIT-enabled) and --nojit (HOLE #2 reproduces without JIT).
    run_cratonvm_and_assert(&bin, &java_home, &classes, &expected, &[]);
    run_cratonvm_and_assert(&bin, &java_home, &classes, &expected, &["--nojit"]);
}
