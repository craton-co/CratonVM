// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A receiver-resolved guarded inline must refuse a body that a registered
//! native shadows BELOW its declaring class.
//!
//! `NativeMethodRegistry::find` is keyed on the EXACT class name, and the
//! collection-carrier scheme registers its natives on the CONCRETE receiver
//! class while the JDK declares the body on a supertype.
//! `resolve_inline_site_from` (`vm/src/runtime/interpreter/jit_bridge.rs`)
//! asked only about the constant-pool class and the DECLARING class, so for a
//! CratonVM-minted `java/util/TreeMap$EntryIterator` it refused
//! `Iterator.next()` -- declared on the carrier, where the native is -- and
//! admitted `Iterator.hasNext()` on the SAME guard class, declared one level up
//! on `TreeMap$PrivateEntryIterator`. The spliced JDK body walks a `next` chain
//! the carrier never populates and reports the collection exhausted, so a
//! compiled `for (e : treeMap.tailMap(k).entrySet())` iterated ZERO entries
//! over a six-entry view.
//!
//! `internal/fixed-bugs/guarded-inline-native-screen-asked-the-declaring-class-FIXED-20260904.md`;
//! `org.h2.test.store.TestRandomMapOps` op:1033, which failed H2 in 11-22 s.
//!
//! # Why this test spawns the binary
//!
//! `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` is the feature's gate and it is
//! **default-off**, so the defect is invisible to any arm that does not set it
//! -- `RJitTreeSubMapIter` in the regression suite covers this exact Java shape
//! under the shipped default (`run.sh` sets no environment) and so could not
//! see it: with the flag off no guarded site is ever planned. A flag has to be set before
//! the process starts to be seen by the memoising `env_cache` reader, and the
//! collection views this needs (a natively-managed `TreeMap`, its minted
//! `TreeMap$EntrySet`, that view's minted iterator) do not come up in the
//! in-process `Vm::new(VmConfig::new())` that the sibling
//! `pgo02_guarded_virtual_inline.rs` drives: there `entrySet().iterator()`
//! answers `null` before any of this is reached. So: a real binary, a real
//! JDK, one env var.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

/// Iterations. The divergence appears around 500, when `iterTail` compiles;
/// 3000 leaves a wide margin on a loaded host without making the test slow.
const ITERS: &str = "3000";

/// A compiled TreeMap probe finishes in about a second. Generous, because a
/// loaded CI host is exactly when it is slowest.
const TIMEOUT: Duration = Duration::from_secs(300);

fn probe_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("jit_guarded_inline_native_shadow_fixtures")
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

/// Compile the fixture if the checked-in `.class` is missing.
///
/// A `javac` that cannot be LAUNCHED is the one legitimate skip. A `javac` that
/// ran and rejected the source is a broken fixture and must fail loudly --
/// reporting that as "toolchain missing" is how a test becomes a permanent
/// vacuous pass.
fn ensure_probe_compiled() -> bool {
    let dir = probe_dir();
    let class_file = dir.join("JitGuardedInlineNativeShadowProbe.class");
    if class_file.exists() {
        return true;
    }
    let source = dir.join("JitGuardedInlineNativeShadowProbe.java");
    if !source.exists() {
        return false;
    }
    let compile = Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&dir)
        .arg(&source)
        .output();
    match compile {
        Err(_) => false,
        Ok(o) => {
            if !o.status.success() {
                let stderr = String::from_utf8_lossy(&o.stderr);
                if stderr.contains("release version") && stderr.contains("not supported") {
                    eprintln!(
                        "[jit_guarded_inline_native_shadow] javac cannot target --release 21; \
                         skipping. Point JAVA_HOME at a JDK 21+ install."
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[jit_guarded_inline_native_shadow] the checked-in fixture failed to compile \
                 -- fix the .java source. javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            class_file.exists()
        }
    }
}
