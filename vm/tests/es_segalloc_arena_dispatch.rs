// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ES-SEGALLOC regression -- `java.lang.foreign.SegmentAllocator.allocate(long,
//! long)` interface dispatch on a CratonVM synthetic `Arena` receiver.
//!
//! Pre-fix symptom (docs/known-issues/elasticsearch-suite/
//! ES-FAIL-20260711-foreign-segmentallocator-dispatch.md): any call that
//! routes through a `SegmentAllocator` default method inherited by `Arena`
//! (`allocate(long)` or `allocate(MemoryLayout)`, both real-JDK bytecode)
//! threw
//!
//!   java.lang.AbstractMethodError: method
//!   java/lang/foreign/SegmentAllocator.allocate(JJ)Ljava/lang/foreign/MemorySegment;
//!   has no Code attribute
//!
//! instead of dispatching to the registered `Arena.allocate(JJ)` native
//! (native-builtins/src/panama.rs::register_pe_arena). This blocked
//! Elasticsearch's `JdkPosixCLibrary` / `NativeAccessHolder` /
//! `BootstrapForTesting` native-access bootstrap entirely, under both JIT-on
//! and JIT-off.
//!
//! Root cause: `Arena` is itself an interface, but CratonVM's
//! `Arena.ofAuto()/ofConfined()/ofShared()/global()` factories
//! (native-builtins/src/panama.rs) allocate their return value under the
//! literal interface class name `java/lang/foreign/Arena`. The C25
//! interface-retarget logic in `vm/src/vm/vm_exec.rs::invoke_on_class_shared_inner`
//! only re-points dispatch onto a receiver's own runtime class when that
//! class is *concrete* (`!c.is_interface()`) -- a real invariant for ordinary
//! Java objects, but one CratonVM's own interface-stamped synthetic receivers
//! violate. So `class_id` never left `SegmentAllocator`, and the follow-up
//! "check the receiver's own native registry" rescue (guarded by
//! `class_id != declaring_id`) never fired either, since both stayed equal to
//! `SegmentAllocator`.
//!
//! Fix: `invoke_on_class_shared_inner` now also recomputes the receiver's
//! *actual* runtime class directly from `args[0]` (independent of whether the
//! interface-exclusion above retargeted `class_id`) and checks its native
//! registry -- generalising the existing "receiver's own-class native
//! rescue" to interface-stamped synthetic receivers, not just concrete ones.
//!
//! This test compiles a tiny Java 21 FFM program that reproduces the
//! Elasticsearch shape (`Arena.allocate(long)` and `Arena.allocate(MemoryLayout)`,
//! the same two default methods `SegmentAllocator.java` uses) and runs it
//! through the `cratonvm` CLI against a real JDK 21 image. It skips
//! gracefully (reports "skip") when `javac`, a real JDK 21 java-home, or the
//! CLI binary is unavailable, so it never misattributes a missing toolchain
//! as a failure.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const PROBE_SRC: &str = r#"
import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;

public class ArenaSegmentAllocatorDispatchProbe {
    public static void main(String[] a) throws Exception {
        // Shape 1: SegmentAllocator.allocate(long) default method --
        // internally does `this.allocate(byteSize, 1)` via invokeinterface
        // against the abstract SegmentAllocator.allocate(long,long).
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(64L);
            System.out.println("confined.allocate(long).byteSize=" + seg.byteSize());
        }

        // Shape 2: SegmentAllocator.allocate(MemoryLayout) default method --
        // the exact Elasticsearch JdkPosixCLibrary/NativeAccessHolder shape
        // (Arena.ofAuto() then allocate(MemoryLayout), which internally does
        // `this.allocate(layout.byteSize(), layout.byteAlignment())`).
        Arena auto = Arena.ofAuto();
        MemoryLayout layout = ValueLayout.JAVA_INT;
        MemorySegment seg2 = auto.allocate(layout);
        System.out.println("auto.allocate(MemoryLayout).byteSize=" + seg2.byteSize());

        // Shape 3: same MemoryLayout-overload shape via a shared arena.
        try (Arena shared = Arena.ofShared()) {
            MemoryLayout layout2 = ValueLayout.JAVA_LONG;
            MemorySegment seg3 = shared.allocate(layout2);
            System.out.println("shared.allocate(MemoryLayout).byteSize=" + seg3.byteSize());
        }

        System.out.println("OK");
    }
}
"#;

fn cratonvm_binary() -> Option<PathBuf> {
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

/// Resolve a real JDK 21+ home directory (needed because `java.lang.foreign`
/// is a preview API whose default methods only carry real Code in an actual
/// JDK image -- CratonVM's synthetic-JDK stubs do not reproduce this bug
/// shape). Checks `CRATONVM_TEST_JAVA_HOME`, then `JAVA_HOME`, then a couple
/// of conventional install locations used elsewhere in this repo's tests.
///
/// Only requires `bin/java` to exist, not `bin/javac` -- some hosts (e.g. a
/// `openjdk-21-jre-headless` package) ship `jdk.compiler` as a module inside
/// `java` itself (reachable via `java --module jdk.compiler/com.sun.tools.
/// javac.Main`, see `compile_probe`) without installing the standalone
/// `javac` launcher binary at all.
fn java_home() -> Option<PathBuf> {
    let java_name = if cfg!(windows) { "java.exe" } else { "java" };
    if let Ok(h) = std::env::var("CRATONVM_TEST_JAVA_HOME") {
        let p = PathBuf::from(h);
        if p.join("bin").join(java_name).exists() {
            return Some(p);
        }
    }
    if let Ok(h) = std::env::var("JAVA_HOME") {
        let p = PathBuf::from(h);
        if p.join("bin").join(java_name).exists() {
            return Some(p);
        }
    }
    for candidate in [
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot",
        "/usr/lib/jvm/java-21-openjdk-amd64",
    ] {
        let p = PathBuf::from(candidate);
        if p.join("bin").join(java_name).exists() {
            return Some(p);
        }
    }
    None
}

/// Compile `src` (full source text) as `name.java` into `out_dir`, then
/// normalise the resulting class file's minor version so CratonVM will load
/// it (see below). Returns `false` if compilation is unavailable or failed
/// (caller skips).
///
/// `java.lang.foreign` is a JDK 21 preview API. Two compile strategies are
/// tried:
///   1. Standalone `javac --release 21 --enable-preview` (works whenever a
///      full JDK is installed, e.g. the Windows dev-machine convention used
///      elsewhere in this file).
///   2. `java --module jdk.compiler/com.sun.tools.javac.Main -source 21
///      --enable-preview` -- some hosts only ship a JRE-headless image
///      without the standalone `javac` launcher, but still bundle the
///      `jdk.compiler` module inside `java` itself and can invoke its main
///      class directly. `-source 21` (not `--release 21`) avoids needing the
///      `lib/ct.sym` cross-release symbol data that a minimal JRE image may
///      omit.
///
/// Either path stamps the output class file's minor version to the preview
/// marker `0xFFFF` (JVMS 4.1). CratonVM's `ClassFileVersion::is_supported`
/// only accepts that marker when major == its own `MAX_SUPPORTED` (currently
/// Java 25) -- a JDK-21-preview-marked class file is otherwise rejected as
/// "unsupported class file version", even though the underlying bytecode is
/// valid and CratonVM already implements the referenced JDK 21 API surface
/// (proven by the real Elasticsearch fixture this bug was found against).
/// The real Elasticsearch `.class` files sidestep this because ES's own
/// Gradle toolchain compiles with a newer JDK whose `java.lang.foreign` is no
/// longer preview-annotated, targeting `--release 21`, which produces a
/// plain (minor=0) class file. Mirror that shape here by zeroing the minor
/// version byte pair post-compile -- this is a compile-output normalisation,
/// not a change to the compiled bytecode itself.
fn compile_probe(jh: &Path, out_dir: &Path, name: &str, src: &str) -> bool {
    let java_file = out_dir.join(format!("{name}.java"));
    if let Ok(mut f) = std::fs::File::create(&java_file) {
        if f.write_all(src.trim_start().as_bytes()).is_err() {
            return false;
        }
    } else {
        return false;
    }
    let java_name = if cfg!(windows) { "java.exe" } else { "java" };
    let javac_name = if cfg!(windows) { "javac.exe" } else { "javac" };
    let javac = jh.join("bin").join(javac_name);
    let compiled = if javac.exists() {
        Command::new(&javac)
            .arg("--release")
            .arg("21")
            .arg("--enable-preview")
            .arg("-d")
            .arg(out_dir)
            .arg(&java_file)
            .output()
    } else {
        // Fall back to invoking the `jdk.compiler` module's main class
        // directly through `java` (JRE-headless images without a standalone
        // `javac` binary still usually carry this module).
        Command::new(jh.join("bin").join(java_name))
            .arg("--module")
            .arg("jdk.compiler/com.sun.tools.javac.Main")
            .arg("-source")
            .arg("21")
            .arg("--enable-preview")
            .arg("-d")
            .arg(out_dir)
            .arg(&java_file)
            .output()
    };
    match compiled {
        // Neither compiler could be launched — the one legitimate skip.
        Err(_) => return false,
        // The compiler RAN and rejected the source: answering `false` here reads
        // to the caller as "no javac, skip", which makes this test a permanent
        // vacuous pass.
        Ok(o) => assert!(
            o.status.success(),
            "[es_segalloc_arena_dispatch] the embedded probe failed to compile — fix \
             the probe source. compiler stderr:\n{}",
            String::from_utf8_lossy(&o.stderr)
        ),
    }
    let class_file = out_dir.join(format!("{name}.class"));
    if !class_file.exists() {
        return false;
    }
    strip_preview_minor_version(&class_file);
    true
}

/// Zero a class file's minor-version field (bytes 4-5, big-endian u16) if it
/// carries the JVMS 4.1 preview marker `0xFFFF`. No-op (and never fails the
/// caller) if the file is missing, too short, or already has minor=0 -- this
/// is a best-effort normalisation, see `compile_probe`'s doc comment.
fn strip_preview_minor_version(class_file: &Path) {
    let Ok(mut bytes) = std::fs::read(class_file) else {
        return;
    };
    if bytes.len() < 8 || &bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        return;
    }
    if bytes[4] == 0xFF && bytes[5] == 0xFF {
        bytes[4] = 0;
        bytes[5] = 0;
        let _ = std::fs::write(class_file, &bytes);
    }
}

fn temp_classes_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cratonvm-essegalloc-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn arena_segment_allocator_default_methods_dispatch_to_native() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[es-segalloc] cratonvm binary not found; build with \
                 `cargo build --release -p cratonvm-cli`; skipping"
            );
            return;
        }
    };
    let jh = match java_home() {
        Some(j) => j,
        None => {
            eprintln!(
                "[es-segalloc] no real JDK 21+ home found (set \
                 CRATONVM_TEST_JAVA_HOME or JAVA_HOME); skipping"
            );
            return;
        }
    };
    let classes = temp_classes_dir("probe");
    if !compile_probe(
        &jh,
        &classes,
        "ArenaSegmentAllocatorDispatchProbe",
        PROBE_SRC,
    ) {
        eprintln!(
            "[es-segalloc] failed to compile ArenaSegmentAllocatorDispatchProbe \
             (javac --release 21 --enable-preview); skipping"
        );
        return;
    }

    let output = Command::new(&bin)
        .arg("--java-home")
        .arg(&jh)
        .arg("--nojit")
        .arg("-cp")
        .arg(&classes)
        .arg("ArenaSegmentAllocatorDispatchProbe")
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[es-segalloc] failed to spawn cratonvm: {e}; skipping");
            return;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("STDOUT:\n{stdout}\n--- STDERR ---\n{stderr}");

    for needle in [
        "confined.allocate(long).byteSize=64",
        "auto.allocate(MemoryLayout).byteSize=4",
        "shared.allocate(MemoryLayout).byteSize=8",
    ] {
        assert!(
            combined.contains(needle),
            "ArenaSegmentAllocatorDispatchProbe (--nojit) missing line `{needle}`. Output:\n{combined}"
        );
    }
    assert!(
        combined.contains("\nOK") || combined.trim_end().ends_with("OK"),
        "ArenaSegmentAllocatorDispatchProbe (--nojit) never printed final OK marker. Output:\n{combined}"
    );
    assert!(
        !combined.contains("AbstractMethodError") && !combined.contains("has no Code attribute"),
        "ArenaSegmentAllocatorDispatchProbe (--nojit) regressed: SegmentAllocator.allocate(JJ) \
         resolved to the abstract interface declaration instead of the Arena native.\n{combined}"
    );

    // Same probe with the JIT enabled -- the bug reproduced identically under
    // both JIT-on and JIT-off, so pin both.
    let output_jit = Command::new(&bin)
        .arg("--java-home")
        .arg(&jh)
        .arg("-cp")
        .arg(&classes)
        .arg("ArenaSegmentAllocatorDispatchProbe")
        .output();
    let output_jit = match output_jit {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[es-segalloc] failed to spawn cratonvm (jit): {e}; skipping jit leg");
            return;
        }
    };
    let stdout_jit = String::from_utf8_lossy(&output_jit.stdout);
    let stderr_jit = String::from_utf8_lossy(&output_jit.stderr);
    let combined_jit = format!("STDOUT:\n{stdout_jit}\n--- STDERR ---\n{stderr_jit}");

    for needle in [
        "confined.allocate(long).byteSize=64",
        "auto.allocate(MemoryLayout).byteSize=4",
        "shared.allocate(MemoryLayout).byteSize=8",
    ] {
        assert!(
            combined_jit.contains(needle),
            "ArenaSegmentAllocatorDispatchProbe (jit) missing line `{needle}`. Output:\n{combined_jit}"
        );
    }
    assert!(
        combined_jit.contains("\nOK") || combined_jit.trim_end().ends_with("OK"),
        "ArenaSegmentAllocatorDispatchProbe (jit) never printed final OK marker. Output:\n{combined_jit}"
    );
    assert!(
        !combined_jit.contains("AbstractMethodError") && !combined_jit.contains("has no Code attribute"),
        "ArenaSegmentAllocatorDispatchProbe (jit) regressed: SegmentAllocator.allocate(JJ) \
         resolved to the abstract interface declaration instead of the Arena native.\n{combined_jit}"
    );
}
