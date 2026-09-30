// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A COMPILED allocation must clear every SoftReference before it throws
//! `OutOfMemoryError` (`java.lang.ref.SoftReference`: "All soft references to
//! softly-reachable objects are guaranteed to have been cleared before the
//! virtual machine throws an OutOfMemoryError").
//!
//! `tools/probes/interp/L5/SoftRefBeforeOome` printed `cleared false` with the
//! JIT on and `cleared true` with `--nojit`, from round 11 wave 8 on (when the
//! probe's allocating loop, which has a call and an exception table, started
//! to OSR-compile). Two defects, both in `vm/src/jit/helpers.rs`:
//!
//! 1. The ROOT: the loop's `soft.get()` is a native call, and compiled code
//!    never consumed the `native_pending_return` handoff root the native
//!    funnel leaves behind. The referent stayed a GC root until the next
//!    native call, so every collection the loop's allocations ran -- the
//!    last-ditch one that condemns every soft reference included -- found it
//!    strongly reachable and restored it. Fixed by
//!    `drain_native_return_at_compiled_helper_entry`. The loop below keeps the
//!    `soft.get()` for exactly this reason: without it the probe passes on the
//!    unfixed binary.
//! 2. The three JIT allocation helpers (`jit_newarray`, `jit_anewarray_object`,
//!    `jit_new_object`) threw as soon as the GC-overhead limit latched,
//!    skipping the soft-reference rung the interpreter's
//!    `collect_and_retry_with_thread` runs on the same exit
//!    (`jit_overhead_limit_clear_soft_refs`).
//!
//! Page:
//! `docs/internal/fixed-bugs/r11-orch-jit-overhead-limit-oome-skips-the-soft-reference-rung-FIXED-20260925.md`.
//!
//! One run per helper: a primitive array, a reference array and a plain
//! object. HotSpot 25 prints `oome=true` and `cleared=true` for each.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const PROBE_SRC: &str = r#"
import java.lang.ref.SoftReference;
import java.util.ArrayList;
import java.util.List;

public class JitSoftRefBeforeOome {
    static final class Node { long a, b, c, d, e, f, g, h; Node next; }
    static long touched;

    // The allocating loop runs long enough to be OSR-compiled, so the
    // allocation that finally fails is a compiled one.
    static boolean fill(String mode, SoftReference<byte[]> soft) {
        List<Object> hold = new ArrayList<>();
        try {
            while (true) {
                switch (mode) {
                    case "prim": hold.add(new long[1024]); break;
                    case "ref": hold.add(new Object[1024]); break;
                    default: {
                        Node n = null;
                        for (int i = 0; i < 64; i++) { Node m = new Node(); m.next = n; n = m; }
                        hold.add(n);
                    }
                }
                if (soft.get() != null) touched++;
            }
        } catch (OutOfMemoryError e) {
            hold = null;
            return true;
        }
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "prim";
        SoftReference<byte[]> soft = new SoftReference<>(new byte[16 << 20]);
        boolean oome = fill(mode, soft);
        System.out.println("mode=" + mode);
        System.out.println("oome=" + oome);
        System.out.println("cleared=" + (soft.get() == null));
    }
}
"#;

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
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target");
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

fn jdk_home() -> Option<PathBuf> {
    common::require_jdk(jdk_home_lookup())
}

fn jdk_home_lookup() -> Option<PathBuf> {
    for var in &["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(j) = std::env::var(var) {
            let p = PathBuf::from(&j);
            if p.exists() {
                return Some(p);
            }
        }
    }
    for cand in [
        "/home/victor/jdk25",
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot",
        "C:/Program Files/Java/jdk-25",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn compile_probe(jdk: &Path) -> Option<PathBuf> {
    let javac = jdk.join(if cfg!(windows) {
        "bin/javac.exe"
    } else {
        "bin/javac"
    });
    let dir = std::env::temp_dir().join("cratonvm-jit-alloc-oome-soft-refs-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("JitSoftRefBeforeOome.java");
    let _ = std::fs::remove_file(dir.join("JitSoftRefBeforeOome.class"));
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(&javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[jit_alloc_oome_clears_soft_refs] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    assert!(
        out.status.success() && dir.join("JitSoftRefBeforeOome.class").exists(),
        "the embedded probe failed to compile. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_mode(bin: &Path, jdk: &Path, classes: &Path, mode: &str) -> String {
    let child = Command::new(bin)
        .arg("--java-home")
        .arg(jdk)
        .arg("-Xmx64m")
        .arg("--compatible")
        .arg("-c")
        .arg(classes)
        .arg("JitSoftRefBeforeOome")
        .arg(mode)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cratonvm");
    let done = common::wait_draining(child, Duration::from_secs(240));
    let stdout = String::from_utf8_lossy(&done.output.stdout).into_owned();
    assert!(!done.timed_out, "[{mode}] probe timed out.\nstdout:\n{stdout}");
    stdout
}

#[test]
fn every_jit_allocation_helper_clears_soft_references_before_oome() {
    let Some(bin) = cratonvm_binary() else {
        return;
    };
    let Some(jdk) = jdk_home() else {
        return;
    };
    let Some(classes) = compile_probe(&jdk) else {
        return;
    };
    for mode in ["prim", "ref", "obj"] {
        let stdout = run_mode(&bin, &jdk, &classes, mode);
        assert!(
            stdout.contains("oome=true"),
            "[{mode}] the probe never reached OutOfMemoryError.\nstdout:\n{stdout}"
        );
        assert!(
            stdout.contains("cleared=true"),
            "[{mode}] OutOfMemoryError was thrown with the soft-held block still \
             reachable: a JIT allocation helper skipped the soft-reference rung, \
             or the loop's last `soft.get()` return is still rooted in \
             native_pending_return.\n\
             stdout:\n{stdout}"
        );
    }
}
