// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ES-SEGALLOC regression -- `java.lang.foreign.SegmentAllocator.allocate(long,
//! long)` interface dispatch on a CratonVM synthetic `Arena` receiver.
//!
//! Pre-fix symptom (ES-FAIL-20260711-foreign-segmentallocator-dispatch-FIXED.md): any call that
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
//! This test compiles a tiny FFM program that reproduces the Elasticsearch
//! shape (`Arena.allocate(long)` and `Arena.allocate(MemoryLayout)`, the same
//! two default methods `SegmentAllocator.java` uses) and runs it through the
//! `cratonvm` CLI against a real JDK image. `compile_probe` picks the release
//! and preview flags from what the toolchain accepts -- see its doc comment;
//! JDK 21 is the floor, not the target. It skips gracefully (reports "skip")
//! when a real java-home or the CLI binary is unavailable, so it never
//! misattributes a missing toolchain as a failure -- but a compiler that RUNS
//! and rejects the source is a hard failure, never a skip.

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

mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
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

/// Resolve a real JDK 21+ home directory (needed because the
/// `java.lang.foreign` default methods only carry real Code in an actual JDK
/// image -- CratonVM's synthetic-JDK stubs do not reproduce this bug shape).
/// 21 is the floor because that is the first release with FFM at all; the API
/// stopped being preview in 22, which is what `compile_probe`'s flag ladder
/// is about. Checks `CRATONVM_TEST_JAVA_HOME`, then `JAVA_HOME`, then a couple
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
/// `java.lang.foreign` was preview in JDK 21 and **final since JDK 22**
/// (JEP 454), so the flags this needs depend on the toolchain `jh` points at.
/// Each branch below therefore tries the non-preview form FIRST and falls back
/// to the JDK-21 preview form; the first strategy that compiles wins, and the
/// assertion only fires when every strategy ran and rejected the source.
///
///   * standalone `javac` present: `--release 22`, else
///     `--release 21 --enable-preview`.
///   * no standalone `javac` (an `openjdk-*-jre-headless` image still bundles
///     the `jdk.compiler` module inside `java` itself): the module's main
///     class at the toolchain's own default release, else with
///     `-source 21 --enable-preview`. `-source` rather than `--release`
///     avoids needing the `lib/ct.sym` cross-release symbol data such a
///     minimal image may omit.
///
/// **Both flag pairs are load-bearing; neither works on both toolchains, and
/// there is no third option that does.** Measured on Adoptium 25 (2026-08-18):
///
/// | flags | JDK 25 |
/// |---|---|
/// | `--release 21 --enable-preview` | `error: invalid source release 21 with --enable-preview` |
/// | `--release 21` (drop the flag) | `error: Arena is a preview API and is disabled by default` |
/// | `--release 22` | compiles, major 66 minor 0 |
///
/// The middle row is the one worth remembering: `ct.sym` records
/// `java.lang.foreign` as preview *for release 21* no matter how new the
/// compiler is, so dropping `--enable-preview` while keeping `--release 21`
/// does not work. An earlier version of this comment asserted the opposite --
/// that Elasticsearch's own Gradle toolchain gets a plain class file from a
/// newer JDK at `--release 21` -- and that claim is false. What ES actually
/// relies on is compiling against a *finalised* FFM, i.e. release >= 22, which
/// is what the primary strategy now does.
///
/// The valid release window is therefore `22 ..= 25`: at least 22 for a
/// non-preview `java.lang.foreign`, at most CratonVM's
/// `ClassFileVersion::MAX_SUPPORTED` (Java 25, major 69). 22 is chosen because
/// it stays inside that window as the ceiling rises and keeps the widest
/// toolchain compatibility.
///
/// The JDK-21 fallback path still stamps the output class file's minor version
/// to the preview marker `0xFFFF` (JVMS 4.1), which CratonVM's
/// `ClassFileVersion::verify` only accepts when major == its own
/// `MAX_SUPPORTED` -- a JDK-21-preview-marked class file is otherwise rejected
/// as "unsupported class file version", even though the bytecode is valid and
/// CratonVM implements the referenced API surface (proven by the real
/// Elasticsearch fixture this bug was found against). So the post-compile
/// minor-version zeroing below is still live, for that path only -- it is a
/// compile-output normalisation, not a change to the compiled bytecode.
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
    // Non-preview form first, JDK-21 preview form second — see the doc comment
    // for why both are needed and why there is no single flag set that works on
    // every toolchain.
    let (program, attempts): (PathBuf, [(&str, &[&str]); 2]) = if javac.exists() {
        (
            javac,
            [
                ("javac --release 22", &["--release", "22"]),
                (
                    "javac --release 21 --enable-preview",
                    &["--release", "21", "--enable-preview"],
                ),
            ],
        )
    } else {
        // Fall back to invoking the `jdk.compiler` module's main class
        // directly through `java` (JRE-headless images without a standalone
        // `javac` binary still usually carry this module).
        (
            jh.join("bin").join(java_name),
            [
                (
                    "java --module jdk.compiler (default release)",
                    &["--module", "jdk.compiler/com.sun.tools.javac.Main"],
                ),
                (
                    "java --module jdk.compiler -source 21 --enable-preview",
                    &[
                        "--module",
                        "jdk.compiler/com.sun.tools.javac.Main",
                        "-source",
                        "21",
                        "--enable-preview",
                    ],
                ),
            ],
        )
    };

    let mut rejections = String::new();
    let mut compiled_ok = false;
    for (label, args) in attempts {
        match Command::new(&program)
            .args(args)
            .arg("-d")
            .arg(out_dir)
            .arg(&java_file)
            .output()
        {
            // The compiler could not be launched at all — the one legitimate
            // skip. The second strategy runs the same program, so it cannot
            // launch either.
            Err(_) => return false,
            Ok(o) if o.status.success() => {
                compiled_ok = true;
                break;
            }
            Ok(o) => {
                rejections.push_str(&format!(
                    "\n--- {label} ---\n{}",
                    String::from_utf8_lossy(&o.stderr)
                ));
            }
        }
    }
    // Every strategy RAN and rejected the source. Returning `false` here would
    // read to the caller as "no javac, skip", which makes this test a permanent
    // vacuous pass — so assert, and print what each strategy said.
    assert!(
        compiled_ok,
        "[es_segalloc_arena_dispatch] the embedded probe failed to compile under \
         every strategy — fix the probe source or the release flags. \
         compiler stderr:{rejections}"
    );
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
        // `compile_probe` returns false ONLY when no compiler could be
        // launched at all; a compiler that ran and rejected the source
        // asserts inside it rather than reaching here.
        eprintln!(
            "[es-segalloc] no launchable compiler in the resolved java-home \
             (neither `bin/javac` nor `java --module jdk.compiler`); skipping"
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
