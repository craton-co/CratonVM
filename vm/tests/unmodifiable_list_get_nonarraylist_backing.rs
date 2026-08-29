// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression pin: `Collections.unmodifiableList(x).get(i)` must read the
//! element for EVERY backing list, not only the ArrayList/Vector-shaped ones.
//!
//! `native_unmod_get` (`native-collections/src/lib.rs`) bounds-checks the index
//! before delegating so the view keeps throwing
//! `ArrayIndexOutOfBoundsException` rather than the backing ArrayList's plain
//! `IndexOutOfBoundsException`. It took the size from `al_state`, which reports
//! `(None, 0)` for two different situations: an empty ArrayList, and a receiver
//! whose `elementData`/`size` slots it must not read at all — a `LinkedList`, a
//! `Collections$SingletonList`, any foreign `List`. Against that second 0 the
//! check rejected **every** index, so a one-element view answered `size()=1`,
//! `isEmpty()=false`, iterated its element, and threw from `get(0)`.
//!
//! Found through `org.springframework.test.context.aot.AotIntegrationTests`,
//! which fails both its tests on it: QDox's `JavaSource.getClasses()` is an
//! unmodifiable view over a non-ArrayList list, so Spring's
//! `SourceFile.getClassName` passes `Assert.state(getClasses().size() == 1)`
//! and then dies on `getClasses().get(0)`.
//!
//! The probe pins both halves: the reads that must succeed, and the
//! out-of-range reads that must still throw — a fix that simply deleted the
//! bounds check would pass the first half and fail the second.
//!
//! Strategy is `sublist_view_regression.rs`'s: compile a self-contained probe,
//! run it under HotSpot and under CratonVM (default and `--nojit`), and require
//! byte-identical stdout.

use std::path::{Path, PathBuf};
use std::process::Command;

const PROBE_SRC: &str = r#"import java.util.*;

public class UnmodListGetProbe {
    static String call(java.util.concurrent.Callable<Object> c) {
        try { return String.valueOf(c.call()); }
        catch (Throwable t) { return "THREW " + t.getClass().getName(); }
    }

    static void probe(String label, List<String> backing) {
        List<String> u = Collections.unmodifiableList(backing);
        System.out.println(label + " size=" + u.size()
                + " isEmpty=" + u.isEmpty()
                + " get(0)=" + call(() -> u.get(0))
                + " getOOB=" + call(() -> u.get(u.size()))
                + " getNeg=" + call(() -> u.get(-1))
                + " listItr=" + call(() -> { Iterator<String> i = u.listIterator(0); return i.next(); })
                + " subList=" + call(() -> u.subList(0, 1)));
    }

    public static void main(String[] args) {
        probe("arraylist  ", new ArrayList<String>(Arrays.asList("a", "b")));
        probe("linkedlist ", new LinkedList<String>(Arrays.asList("a", "b")));
        probe("vector     ", new Vector<String>(Arrays.asList("a", "b")));
        probe("arraysAsList", Arrays.asList("a", "b"));
        probe("singleton  ", Collections.singletonList("a"));
        probe("cow        ", new java.util.concurrent.CopyOnWriteArrayList<String>(Arrays.asList("a", "b")));
        probe("foreign    ", new Foreign());
        // The empty list must still reject index 0 — the arm the bounds check
        // exists for, and the one a "just delete the check" fix breaks.
        probe("emptyAL    ", new ArrayList<String>());
        probe("emptyLL    ", new LinkedList<String>());
        System.out.println("OK");
    }

    /** A List that is neither ArrayList- nor Vector-shaped and is not a JDK class. */
    static class Foreign extends AbstractSequentialList<String> {
        private final List<String> d = new ArrayList<String>(Arrays.asList("a", "b"));
        public ListIterator<String> listIterator(int i) { return d.listIterator(i); }
        public int size() { return d.size(); }
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
    let src_path = classes.join("UnmodListGetProbe.java");
    std::fs::write(&src_path, PROBE_SRC).ok()?;
    let out = Command::new("javac")
        .args([
            "--release",
            "17",
            "-nowarn",
            "-d",
            classes.to_str().unwrap(),
            src_path.to_str().unwrap(),
        ])
        .output()
        .ok()?;
    // javac REJECTED THE ARGUMENTS, not the source (see
    // `sublist_view_regression.rs` for the full rationale): an unsupported
    // `--release` means this javac never opened the file, which is a missing
    // toolchain, not a broken probe.
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[unmodifiable_list_get] javac cannot target --release 17 ({}); skipping.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    // javac RAN and rejected the source: the probe is broken, and returning
    // None here would read as "javac unavailable, skip" and make this test a
    // permanent vacuous pass.
    assert!(
        out.status.success(),
        "[unmodifiable_list_get] the embedded probe failed to compile — fix the probe source. \
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
        .args(["-cp", classes.to_str().unwrap(), "UnmodListGetProbe"])
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

/// Run the probe under CratonVM and assert clean exit + stdout == HotSpot,
/// EXCEPT for the out-of-range exception CLASS, which CratonVM deliberately
/// reports as the `ArrayIndexOutOfBoundsException` subclass — see
/// `list-out-of-bounds-exception-class-and-message-FIXED-20260806.md`.
fn run_cratonvm_and_assert(bin: &Path, java_home: &str, classes: &Path, extra: &[&str]) -> String {
    let mut cmd = Command::new(bin);
    cmd.args(["--java-home", java_home, "-c", classes.to_str().unwrap()]);
    for a in extra {
        cmd.arg(a);
    }
    cmd.arg("UnmodListGetProbe");
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
        "[{mode}] cratonvm exited non-zero. status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );
    assert!(
        stdout.contains("OK"),
        "[{mode}] probe did not reach OK.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stdout
}

/// Both out-of-bounds classes are `IndexOutOfBoundsException`s; CratonVM funnels
/// every unmodifiable list through one synthetic class and answers with the
/// `ArrayIndexOutOfBoundsException` subclass. Normalise so the comparison below
/// is about VALUES, not about that separately-tracked divergence.
fn normalise(s: &str) -> String {
    s.replace(
        "THREW java.lang.ArrayIndexOutOfBoundsException",
        "THREW <IOOBE>",
    )
    .replace("THREW java.lang.IndexOutOfBoundsException", "THREW <IOOBE>")
    .replace(
        "THREW java.util.NoSuchElementException",
        "THREW <NoSuchElement>",
    )
}

#[test]
fn unmodifiable_list_get_reads_every_backing_shape() {
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

    // Sanity on the oracle: HotSpot must actually READ through every wrapper
    // and must still reject the out-of-range indices. Without this, a probe
    // edit that made every line say `THREW ...` would let both sides "agree".
    for label in [
        "arraylist  ",
        "linkedlist ",
        "vector     ",
        "arraysAsList",
        "singleton  ",
        "cow        ",
        "foreign    ",
    ] {
        let line = expected
            .lines()
            .find(|l| l.starts_with(label))
            .unwrap_or_else(|| panic!("HotSpot baseline missing `{label}`:\n{expected}"));
        assert!(
            line.contains(" get(0)=a"),
            "HotSpot baseline wrong for `{label}`: {line}"
        );
        assert!(
            line.contains(" getOOB=THREW"),
            "HotSpot baseline wrong for `{label}`: {line}"
        );
        assert!(
            line.contains(" getNeg=THREW"),
            "HotSpot baseline wrong for `{label}`: {line}"
        );
    }
    assert!(
        expected
            .lines()
            .any(|l| l.starts_with("emptyAL    ") && l.contains(" get(0)=THREW")),
        "HotSpot baseline wrong for the empty list:\n{expected}"
    );

    let want = normalise(&expected);
    for extra in [&[][..], &["--nojit"][..]] {
        let got = normalise(&run_cratonvm_and_assert(&bin, &java_home, &classes, extra));
        let mode = if extra.is_empty() {
            "default"
        } else {
            "--nojit"
        };
        assert_eq!(
            got, want,
            "[{mode}] CratonVM diverged from HotSpot on unmodifiable-list reads"
        );
    }
}
