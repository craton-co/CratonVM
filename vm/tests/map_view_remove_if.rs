// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `removeIf` through a map's `keySet()` / `entrySet()` / `values()` view.
//!
//! CratonVM mints those views under the JDK's own carrier classes and keeps
//! their state in slots the JDK bodies know nothing about, so
//! `force_native_over_real_jdk_bytecode` lists them and their methods — but a
//! gate entry is not a registration. `removeIf` was listed for the SET-shaped
//! carriers and registered on none of them, so the lookup fell through to the
//! very bytecode the gate exists to avoid. That only bit the one carrier in the
//! family that DECLARES `removeIf`: `ConcurrentHashMap$EntrySetView`, whose
//! body is `return map.removeEntryIf(filter)` over a `map` field a minted view
//! never fills —
//!
//! ```text
//! java.lang.NullPointerException: Cannot invoke
//!   "java.util.concurrent.ConcurrentHashMap.removeEntryIf(java.util.function.Predicate)"
//!   because "this.map" is null
//!     at java.util.concurrent.ConcurrentHashMap$EntrySetView.removeIf(ConcurrentHashMap.java:4856)
//!     at org.springframework.test.context.cache.DefaultContextCache.remove(DefaultContextCache.java:344)
//! ```
//!
//! — which made three `core/spring-boot-test` classes report
//! `containersFailed=1` from a `@DirtiesContext` `afterTestClass` callback
//! while every test inside them passed.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;

const PROBE: &str = "MapViewRemoveIfProbe";
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
            "[map_view_remove_if] cratonvm binary missing; build -p cratonvm-cli or set CRATONVM_BIN"
        );
        return (String::new(), String::new());
    };
    let Some(jh) = java_home() else {
        eprintln!("[map_view_remove_if] JDK 25 java-home missing; skipping");
        return (String::new(), String::new());
    };
    let Some(cp) = classpath_dir() else {
        eprintln!("[map_view_remove_if] {PROBE}.class missing; javac unavailable");
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
                    panic!("[map_view_remove_if] {PROBE} timed out after {TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => panic!("[map_view_remove_if] try_wait failed: {e}"),
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

/// `removeIf` through every map view must write through to the backing map.
#[test]
fn remove_if_through_a_map_view_writes_through_to_the_map() {
    let (stdout, combined) = run_probe();
    if stdout.is_empty() {
        return;
    }
    assert!(
        stdout.contains("MAPVIEW_REMOVEIF_OK"),
        "{PROBE}: `removeIf` through a map view did not change the backing map as the JDK \
         specifies. An NPE on `this.map` means the receiver's own JDK bytecode ran over a \
         CratonVM-minted view — check that `removeIf` is REGISTERED for that carrier and \
         not merely listed in `force_native_over_real_jdk_bytecode`.\n\n{combined}"
    );
}
