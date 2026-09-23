// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.util.Properties` answers for the whole surface it declares, on a
//! receiver whose inherited `Hashtable.table` was never allocated.
//!
//! # What this exists to prevent
//!
//! `Properties` keeps its entries in three places and none of them is the
//! bucket table its fields advertise: a String->String pair lives in a Rust
//! side-table (`native-builtins/src/properties_sidetable.rs`), anything else
//! lives in the real `map` `ConcurrentHashMap`, and the inherited
//! `Hashtable.table` holds nothing at all — `map_carrier_class_for_receiver`
//! records the measurement, `occupied=0` with `size=2`. Since 2026-09-12 that
//! field is also NULL, as it is on HotSpot, where it used to be an eagerly
//! allocated `Object[16]` worth 144 of the 224 bytes an empty `Properties`
//! retained.
//!
//! Two properties of this class make a whole-surface gate the right shape
//! rather than a test of the method a change touches:
//!
//!   * **The failure mode is silence.** A method that consults the wrong store
//!     answers "absent" or "empty"; nothing throws. The defect this file was
//!     written alongside is exactly that shape — `remove` of a non-String key
//!     (or of the empty-String key, the two the side-table cannot spell)
//!     returned `null` without consulting the CHM those keys actually live in,
//!     so the entry survived its own removal and `size()` went on counting it.
//!   * **Which implementation serves a method is a property of registration
//!     ORDER**, not of any one function. `register_properties_sidetable` runs
//!     after `register_collections_natives` and overwrites most of the Map
//!     surface; a triple added to or dropped from either registrar silently
//!     moves a method between two implementations with different backing
//!     stores.
//!
//! The probe is `probes/PropertiesBacking.java`, read from the tree rather than
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

const PROBE: &str = "PropertiesBacking";
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

/// Compile `probes/PropertiesBacking.java` into a temp directory.
///
/// `None` ONLY when javac cannot be launched at all. A javac that RAN and
/// REJECTED the source is a broken probe and panics — skipping there would turn
/// this test into a permanent vacuous pass (`probe_compile_guard.rs`).
fn compile_probe(javac: &Path) -> Option<PathBuf> {
    let src = workspace_root()
        .join("probes")
        .join(format!("{PROBE}.java"));
    if !src.exists() {
        eprintln!("[properties_backing] {src:?} missing; skipping");
        return None;
    }
    let dir = std::env::temp_dir().join("cratonvm-properties-backing");
    let _ = std::fs::create_dir_all(&dir);
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join(format!("{PROBE}.class")));
    let out = match Command::new(javac).arg("-d").arg(&dir).arg(&src).output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[properties_backing] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    assert!(
        out.status.success() && dir.join(format!("{PROBE}.class")).exists(),
        "[properties_backing] {PROBE} failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

/// `(stdout, stdout + stderr)`, or empty strings when a prerequisite is absent.
fn run_probe() -> (String, String) {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[properties_backing] cratonvm binary missing; build -p cratonvm-cli or set \
             CRATONVM_BIN"
        );
        return (String::new(), String::new());
    };
    let Some(javac) = javac_path() else {
        eprintln!("[properties_backing] no javac; skipping");
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
        .unwrap_or_else(|e| panic!("[properties_backing] failed to spawn {bin:?}: {e}"));

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[properties_backing] {PROBE} timed out after {TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => panic!("[properties_backing] try_wait failed: {e}"),
        }
    }

    let output = child
        .wait_with_output()
        .expect("[properties_backing] wait_with_output failed");
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
    (stdout.clone(), format!("{stdout}\n{stderr}"))
}

/// Every method `javap -p java.util.Properties` lists, bar the four the probe's
/// header excludes, against a HotSpot-verified oracle.
#[test]
fn properties_answers_for_its_whole_surface() {
    let (stdout, combined) = run_probe();
    if stdout.is_empty() {
        return;
    }
    // `PROPS_END` separates "the probe ran and disagreed" from "the VM died
    // partway", which report very differently and need different first moves.
    assert!(
        stdout.contains("PROPS_END"),
        "[properties_backing] {PROBE} did not reach its end marker — the VM failed partway \
         rather than the assertions disagreeing.\n\n{combined}"
    );
    assert!(
        stdout.contains("PASS PropertiesBacking"),
        "[properties_backing] {PROBE} disagreed with HotSpot. `Properties` keeps String->String \
         pairs in a Rust side-table (`native-builtins/src/properties_sidetable.rs`), everything \
         else in the real `map` CHM, and NOTHING in the inherited `Hashtable.table` — which is \
         null on a fresh one, here as on HotSpot. A MISMATCH on a non-String or empty-String key \
         means a method consulted only the side-table, which cannot spell those keys, and missed \
         the CHM they live in. A MISMATCH on an ordinary String key means the two stores have \
         drifted apart, most likely because a triple moved between \
         `register_properties_sidetable` and `register_collections_natives`. An `ERROR` row \
         naming a whole section means that method has no working body at all.\n\n{combined}"
    );
}
