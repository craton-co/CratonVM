// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `AbstractCollection.toArray(T[])` over a Map-view iterator.
//!
//! CratonVM services the inherited `toArray(T[])` with a native
//! (`real_jdk_to_array_typed` in `vm/src/vm/vm_init.rs`) because the JDK's own
//! bytecode reads `elementData`, which only `ArrayList` has. When the receiver
//! has no `elementData` the native falls back to walking the receiver's own
//! `iterator()` — and that loop used to dispatch BY NAME:
//!
//! ```ignore
//! ctx.invoke("java/util/LinkedHashMap$LinkedKeyIterator", "hasNext", "()Z", ..)
//! ```
//!
//! A by-name invoke resolves the named class's own bytecode and never consults
//! `should_force_registered_native_over_bytecode`, the gate that exists exactly
//! because a CratonVM-minted `HashMap$KeyIterator` /
//! `LinkedHashMap$LinkedKeyIterator` carries its snapshot PAST the fields the
//! real `HashIterator` bytecode walks. The real `hasNext()` therefore read an
//! unset `next` field and answered `false` on the first element: the loop broke
//! at `i = 0`, `size()` had already fixed the result length, and the caller got
//! a right-length array of nulls.
//!
//! That is one collection shape away from Jetty's
//! `org.eclipse.jetty.util.ClassMatcher` (`AbstractSet<String>` over a private
//! `Map`, `getPatterns()` = `toArray(new String[size()])`), whose all-null
//! pattern set made an empty hidden-class matcher — and an empty
//! `ClassMatcher` matches EVERYTHING, so `WebAppClassLoader` discarded the
//! `org.apache.jasper.servlet.JspServlet` its parent had just resolved and
//! Spring Boot's embedded Jetty failed to start with
//! `UnavailableException: Class loading error for holder jsp==...`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod common;

const PROBE: &str = "MapBackedSetToArrayProbe";
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
fn run_probe(extra_env: &[(&str, &str)]) -> (String, String) {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[abstract_collection_to_array_typed] cratonvm binary missing; \
             build -p cratonvm-cli or set CRATONVM_BIN"
        );
        return (String::new(), String::new());
    };
    let Some(jh) = java_home() else {
        eprintln!("[abstract_collection_to_array_typed] JDK 25 java-home missing; skipping");
        return (String::new(), String::new());
    };
    let Some(cp) = classpath_dir() else {
        eprintln!("[abstract_collection_to_array_typed] {PROBE}.class missing; javac unavailable");
        return (String::new(), String::new());
    };

    let mut cmd = Command::new(&bin);
    cmd.arg("--java-home")
        .arg(&jh)
        .arg("-c")
        .arg(&cp)
        .arg(format!("cratonvm.{PROBE}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn cratonvm toArray probe");

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "[abstract_collection_to_array_typed] {PROBE} timed out after {TIMEOUT:?}"
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => panic!("[abstract_collection_to_array_typed] try_wait failed: {e}"),
        }
    }

    let output = child
        .wait_with_output()
        .expect("collect cratonvm toArray probe output");
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

/// The typed `toArray(T[])` must agree with the receiver's own iterator.
#[test]
fn map_view_backed_collection_to_array_typed_keeps_its_elements() {
    let (stdout, combined) = run_probe(&[]);
    if stdout.is_empty() {
        return;
    }
    assert!(
        stdout.contains("MAPSET_TOARRAY_OK"),
        "{PROBE}: `AbstractCollection.toArray(T[])` disagreed with the receiver's own \
         iterator. A right-length array of nulls means the native's iterator loop broke \
         before the first element — check that it dispatches `hasNext`/`next` with \
         `invoke_virtual` (receiver dispatch, honours the force-native gate) and not \
         `invoke(<class name>, ..)` (resolves the named class's own bytecode).\n\n{combined}"
    );
}

/// Same, with the JIT off: the defect was pure native dispatch, so a `--nojit`
/// arm that still passes rules a compiled body in or out for anyone who
/// re-opens this.
#[test]
fn map_view_backed_collection_to_array_typed_is_not_a_jit_artifact() {
    let (stdout, combined) = run_probe(&[("CRATONVM_DISABLE_JIT", "1")]);
    if stdout.is_empty() {
        return;
    }
    assert!(
        stdout.contains("MAPSET_TOARRAY_OK"),
        "{PROBE} (JIT disabled): `AbstractCollection.toArray(T[])` disagreed with the \
         receiver's own iterator\n\n{combined}"
    );
}
