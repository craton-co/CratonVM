// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The whole gated surface of the map/set view carriers, on every carrier.
//!
//! CratonVM mints `keySet()` / `entrySet()` / `values()` views under the JDK's
//! own carrier classes and keeps their state in slots the JDK bodies know
//! nothing about, so `force_native_over_real_jdk_bytecode` lists those classes
//! and the methods that must not run their own bytecode. **A gate entry is not
//! a registration**: when the gate names a method nothing is registered for,
//! the lookup finds nothing to prefer and falls through to exactly the bytecode
//! the gate exists to avoid.
//!
//! Two of those shipped at once, and only the carriers that DECLARE the method
//! noticed — the rest were inheriting a `Collection` default and are one JDK
//! upgrade away from not doing so, which is why this sweeps the family instead
//! of asserting the two known rows:
//!
//! ```text
//! ConcurrentHashMap$EntrySetView.removeIf  ->  map.removeEntryIf(..)
//!   NullPointerException: ... because "this.map" is null
//!
//! TreeSet.spliterator / TreeMap$KeySet.spliterator  ->  TreeMap.keySpliteratorFor(m)
//!   NullPointerException: ... "TreeMap$NavigableSubMap.keySpliterator()" because "sm" is null
//! ```
//!
//! The first made three `core/spring-boot-test` classes report
//! `containersFailed=1` from a `@DirtiesContext` `afterTestClass` callback
//! (`DefaultContextCache.remove` is `contextMap.entrySet().removeIf(..)`) while
//! every test inside them passed. The second threw for every caller of
//! `treeSet.spliterator()`, unconditionally.
//!
//! Every expectation the probe checks is derived from the collection's own
//! contents, so it needs no HotSpot arm and cannot pass vacuously.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;

const PROBE: &str = "CollectionViewGatedSurfaceProbe";
const TIMEOUT: Duration = Duration::from_secs(60);

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

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
    if let Ok(home) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(home);
        if p.exists() {
            return Some(p);
        }
    }
    for candidate in [
        "C:/Program Files/Java/jdk-25",
        "C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot",
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot",
        "/data/toolchain/jdk-25",
    ] {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn classpath_dir() -> Option<PathBuf> {
    let committed = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources");
    if committed
        .join("cratonvm")
        .join(format!("{PROBE}.class"))
        .exists()
    {
        return Some(committed);
    }
    None
}

/// Run the probe and return `(stdout, stdout + stderr)`; empty when a
/// prerequisite (binary / JDK / compiled fixture) is missing.
fn run_probe() -> (String, String) {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[collection_view_gated_surface] cratonvm binary missing; build -p cratonvm-cli or set CRATONVM_BIN"
        );
        return (String::new(), String::new());
    };
    let Some(jh) = java_home() else {
        eprintln!("[collection_view_gated_surface] JDK 25 java-home missing; skipping");
        return (String::new(), String::new());
    };
    let Some(cp) = classpath_dir() else {
        eprintln!("[collection_view_gated_surface] {PROBE}.class missing; javac unavailable");
        return (String::new(), String::new());
    };

    let mut child = Command::new(&bin)
        .arg("--java-home")
        .arg(&jh)
        .arg("-c")
        .arg(&cp)
        .arg(format!("cratonvm.{PROBE}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cratonvm map-view removeIf probe");

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[collection_view_gated_surface] {PROBE} timed out after {TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => panic!("[collection_view_gated_surface] try_wait failed: {e}"),
        }
    }

    let output = child
        .wait_with_output()
        .expect("collect cratonvm map-view removeIf probe output");
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        output.status.success(),
        "{PROBE} exited with {:?}\n\n{combined}",
        output.status.code()
    );
    (stdout, combined)
}

/// Every gated method on every view carrier must agree with the collection.
#[test]
fn the_gated_view_surface_matches_the_collections_own_contents() {
    let (stdout, combined) = run_probe();
    if stdout.is_empty() {
        return;
    }
    assert!(
        stdout.contains("GATED_SURFACE_OK"),
        "{PROBE}: a gated view method disagreed with its own collection. An NPE naming a \
         null backing field (`this.map`, `sm`, `this$0`) means the receiver's own JDK \
         bytecode ran over a CratonVM-minted view: check that the method is REGISTERED \
         for that carrier and not merely listed in \
         `force_native_over_real_jdk_bytecode`, whose entries do nothing on their own. \
         A silently EMPTY answer is the same defect without the crash.\n\n{combined}"
    );
}
