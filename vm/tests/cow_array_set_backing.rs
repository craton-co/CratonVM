// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.util.concurrent.CopyOnWriteArraySet` is backed by the object the JDK
//! puts under it, and answers for the whole surface it declares.
//!
//! # The defect this exists to prevent
//!
//! `register_hashset_natives` mirrors the entire `java.util.HashSet` surface
//! onto `CopyOnWriteArraySet`, on the stated premise that it is one of
//! "HashSet's real-JDK subclasses that share the same `field 0 = backing map`
//! layout". It is neither: it does not extend `HashSet`, and the one instance
//! field the real class declares is
//! `private final CopyOnWriteArrayList<E> al` — sitting at exactly that slot 0.
//! So `native_hs_init` stored a `LinkedHashMap` in a slot declared to hold a
//! list.
//!
//! The class declares nineteen methods and the registrar carries twenty triples
//! for it, so all but a couple looked correct with a native standing in front of
//! them. `removeIf` is in neither set, and its real body ran:
//!
//! ```text
//!   cowSet.removeIf(p)
//!     -> NoSuchMethodError:
//!        java.util.LinkedHashMap.removeIf(java.util.function.Predicate)
//! ```
//!
//! The shape is what makes it worth a gate rather than a fix: a wrong backing
//! is SILENT for every method a native covers, and the one uncovered method is
//! how it surfaced. A future registration added or dropped changes which
//! methods are silent, so the guard has to be the whole declared surface.
//!
//! It also cost the class its size. A `LinkedHashMap` is 88 bytes empty on this
//! VM where a `CopyOnWriteArrayList` is 48, so an empty `CopyOnWriteArraySet`
//! retained 112 bytes against HotSpot's 56.1, and one holding four elements
//! retained **512** against HotSpot's 88.3.
//!
//! The probe is `probes/CowSetBacking.java`, read from the tree rather than
//! embedded here so there is ONE copy of it: it is also the file a human runs
//! by hand against both VMs, and a second copy in this file would be the one
//! that drifts. HotSpot passes it unchanged, which is what makes it an oracle
//! rather than a description of current behaviour.
//!
//! See `the-synthetic-slot-floor-is-one-number-for-two-layouts-FIXED-20260912.md`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;

const PROBE: &str = "CowSetBacking";
const TIMEOUT: Duration = Duration::from_secs(120);

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

/// Prerequisite gate: only a MISSING binary is reported differently — see
/// `common::require_binary`.
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
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = workspace_root().join("target").join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn javac_path() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "javac.exe" } else { "javac" };
    for var in ["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(home) = std::env::var(var) {
            let p = PathBuf::from(home).join("bin").join(exe);
            if p.exists() {
                return Some(p);
            }
        }
    }
    for candidate in [
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot",
        "C:/Program Files/Java/jdk-25",
        "C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot",
        "/data/toolchain/jdk-25",
        "/usr/lib/jvm/jdk-25",
    ] {
        let p = PathBuf::from(candidate).join("bin").join(exe);
        if p.exists() {
            return Some(p);
        }
    }
    // Last resort: whatever is on PATH. `Command::new` resolves it, and a
    // failure to launch is the one legitimate skip below.
    Some(PathBuf::from("javac"))
}

/// Compile `probes/CowSetBacking.java` into a temp directory.
///
/// `None` ONLY when javac cannot be launched at all. A javac that RAN and
/// REJECTED the source is a broken probe and panics — skipping there would turn
/// this test into a permanent vacuous pass (`probe_compile_guard.rs`).
fn compile_probe(javac: &Path) -> Option<PathBuf> {
    let src = workspace_root()
        .join("probes")
        .join(format!("{PROBE}.java"));
    if !src.exists() {
        eprintln!("[cow_array_set_backing] {src:?} missing; skipping");
        return None;
    }
    let dir = std::env::temp_dir().join("cratonvm-cow-array-set-backing");
    let _ = std::fs::create_dir_all(&dir);
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join(format!("{PROBE}.class")));
    let out = match Command::new(javac).arg("-d").arg(&dir).arg(&src).output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[cow_array_set_backing] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    assert!(
        out.status.success() && dir.join(format!("{PROBE}.class")).exists(),
        "[cow_array_set_backing] {PROBE} failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

/// `(stdout, stdout + stderr)`, or empty strings when a prerequisite is absent.
fn run_probe() -> (String, String) {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[cow_array_set_backing] cratonvm binary missing; build -p cratonvm-cli or set \
             CRATONVM_BIN"
        );
        return (String::new(), String::new());
    };
    let Some(javac) = javac_path() else {
        eprintln!("[cow_array_set_backing] no javac; skipping");
        return (String::new(), String::new());
    };
    let Some(classes) = compile_probe(&javac) else {
        return (String::new(), String::new());
    };

    let mut child = Command::new(&bin)
        .arg("--Xmx")
        .arg("512m")
        .arg("-c")
        .arg(&classes)
        .arg(PROBE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("[cow_array_set_backing] failed to spawn {bin:?}: {e}"));

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[cow_array_set_backing] {PROBE} timed out after {TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => panic!("[cow_array_set_backing] try_wait failed: {e}"),
        }
    }

    let output = child
        .wait_with_output()
        .expect("[cow_array_set_backing] wait_with_output failed");
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
    (stdout.clone(), format!("{stdout}\n{stderr}"))
}

/// Every method `javap -p java.util.concurrent.CopyOnWriteArraySet` lists, plus
/// the four it inherits, against a HotSpot-verified oracle.
#[test]
fn cow_array_set_answers_for_its_whole_surface() {
    let (stdout, combined) = run_probe();
    if stdout.is_empty() {
        return;
    }
    // `COWSET_END` separates "the probe ran and disagreed" from "the VM died
    // partway", which report very differently and need different first moves.
    assert!(
        stdout.contains("COWSET_END"),
        "[cow_array_set_backing] {PROBE} did not reach its end marker — the VM failed \
         partway rather than the assertions disagreeing.\n\n{combined}"
    );
    assert!(
        stdout.contains("PASS CowSetBacking"),
        "[cow_array_set_backing] {PROBE} disagreed with HotSpot. A `NoSuchMethodError` naming \
         `java.util.LinkedHashMap` means `CopyOnWriteArraySet` is map-backed again: the real \
         class keeps a `CopyOnWriteArrayList` in `al`, its ONE instance field, and \
         `cow_set_route` (`native-collections`) is what takes a real receiver to its own \
         bytecode over it. An `ERROR` row naming a whole section means that method has no \
         working body at all in either direction.\n\n{combined}"
    );
}
