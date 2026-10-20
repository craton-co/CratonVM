// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

#[test]
fn primitive_varhandle_reads_preserve_storage_and_fallbacks() {
    check_varhandle_read_fixture(
        "PrimitiveVarHandleReadsProbe",
        "PRIMITIVE-VARHANDLE-READS-OK",
        false,
    );
}

#[test]
fn byte_array_view_reads_preserve_carriers_endianness_and_refusals() {
    check_varhandle_read_fixture("ByteArrayViewReadsProbe", "BYTE-ARRAY-VIEW-READS-OK", true);
}

#[test]
fn byte_buffer_view_reads_preserve_layout_endianness_and_refusals() {
    check_varhandle_read_fixture(
        "ByteBufferViewReadsProbe",
        "BYTE-BUFFER-VIEW-READS-OK",
        true,
    );
}

#[test]
fn byte_buffer_view_writes_preserve_wire_bits_bounds_and_refusals() {
    check_varhandle_read_fixture(
        "ByteBufferViewWritesProbe",
        "BYTE-BUFFER-VIEW-WRITES-OK",
        true,
    );
}

#[test]
fn optimizing_varhandle_reads_preserve_judged_cold_paths() {
    check_varhandle_read_fixture(
        "OptimizingVarHandleReadsProbe",
        "OPTIMIZING-VARHANDLE-READS-OK",
        true,
    );
}

fn check_varhandle_read_fixture(probe: &str, marker: &str, byte_view: bool) {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("target")
        });
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    let binary = std::env::var_os("CRATONVM_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            ["release", "debug"]
                .into_iter()
                .map(|p| target.join(p).join(exe))
                .find(|p| p.is_file())
        });
    let Some(binary) = common::require_binary(binary.filter(|p| p.is_file())) else {
        return;
    };
    let home = ["CRATONVM_TEST_JDK", "CRATONVM_TEST_JAVA_HOME", "JAVA_HOME"]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(PathBuf::from))
        .chain([
            PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"),
            PathBuf::from("/home/victor/jdk25"),
        ])
        .find(|p| {
            p.join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" })
                .is_file()
        });
    let Some(home) = common::require_jdk(home) else {
        return;
    };
    let dir = tempfile::tempdir().expect("probe output directory");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "tests/jit_primitive_varhandle_reads_fixtures/{probe}.java"
    ));
    let javac = home
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let compiled = Command::new(javac)
        .arg("-d")
        .arg(dir.path())
        .arg(source)
        .output()
        .expect("javac");
    assert!(
        compiled.status.success(),
        "the probe failed to compile -- fix the fixture source. javac stderr:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let java = home
        .join("bin")
        .join(if cfg!(windows) { "java.exe" } else { "java" });
    let oracle = Command::new(java)
        // Keep helpful exception messages stable across HotSpot fast-throw tiering.
        .arg("-XX:-OmitStackTraceInFastThrow")
        .arg("-cp")
        .arg(dir.path())
        .arg(probe)
        .output()
        .expect("HotSpot oracle");
    assert!(
        oracle.status.success(),
        "{}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected = String::from_utf8_lossy(&oracle.stdout).replace("\r\n", "\n");
    assert!(expected.contains(marker));
    let collectors: &[&str] = if byte_view {
        &["UseGenerationalGC", "UseG1GC", "UseZGC"]
    } else {
        &[""]
    };
    for collector in collectors {
        for mode in ["--jdk-only", "--compatible"] {
            let arms: &[(bool, bool, bool, bool)] = if probe == "OptimizingVarHandleReadsProbe" {
                &[
                    (false, true, false, false),
                    (false, false, false, false),
                    (true, true, false, false),
                    (false, true, true, false),
                    (false, true, false, true),
                ]
            } else if probe == "ByteBufferViewWritesProbe" {
                &[
                    (false, true, false, false),
                    (false, false, false, false),
                    (true, true, false, false),
                    (false, true, false, true),
                ]
            } else {
                &[
                    (false, true, false, false),
                    (false, false, false, false),
                    (true, true, false, false),
                ]
            };
            for &(nojit, compact, ir_disabled, cache_disabled) in arms {
                let mut cmd = Command::new(&binary);
                cmd.arg(mode);
                if !collector.is_empty() {
                    cmd.arg(format!("-XX:+{collector}"));
                }
                if nojit {
                    cmd.arg("--nojit");
                }
                cmd.arg("--java-home")
                    .arg(&home)
                    .arg("-cp")
                    .arg(dir.path())
                    .arg(probe)
                    .env(
                        "CRATONVM_DBG",
                        if probe == "OptimizingVarHandleReadsProbe" {
                            "jitc,jit-method-stats"
                        } else {
                            "jit-method-stats"
                        },
                    )
                    .env("CRATONVM_INTRINSIC_STATS", "1")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                if probe == "ByteBufferViewWritesProbe" && !nojit {
                    cmd.env("CRATONVM_DBG_JITC", "1")
                        .env("CRATONVM_TIER_C1_THRESHOLD", "100000")
                        .env("CRATONVM_TIER_C2_THRESHOLD", "600")
                        .env("CRATONVM_TIER_C2_MIN_INVOCATIONS", "500")
                        .env("CRATONVM_DBG_IR_COMPILES", "1")
                        .env("CRATONVM_DBG_JIT_DISASM", "ByteBufferViewWritesProbe.write");
                    cmd.env_remove("CRATONVM_JIT_VARHANDLE_WRITE_DIRECT_HELPERS");
                    if cache_disabled {
                        cmd.env("CRATONVM_JIT_VARHANDLE_WRITE_DIRECT_HELPERS", "0");
                    }
                }
                if probe == "ByteBufferViewReadsProbe" && !nojit {
                    cmd.env("CRATONVM_DBG_JITC", "1")
                        .env_remove("CRATONVM_JIT_IR_VH_READ_BINDS")
                        .env("CRATONVM_TIER_C1_THRESHOLD", "100000")
                        .env("CRATONVM_TIER_C2_THRESHOLD", "600")
                        .env("CRATONVM_TIER_C2_MIN_INVOCATIONS", "500")
                        .env("CRATONVM_DBG_IR_COMPILES", "1")
                        .env("CRATONVM_DBG_JIT_DISASM", "ByteBufferViewReadsProbe.read");
                }
                if probe == "OptimizingVarHandleReadsProbe" && nojit {
                    // The interpreter control needs values, faults and GC,
                    // rather than thousands of calls to trigger compilation.
                    // The final output is independent of the warmup count.
                    cmd.arg("100");
                }
                if probe == "OptimizingVarHandleReadsProbe" && !nojit {
                    cmd.env("CRATONVM_TIER_C1_THRESHOLD", "100000")
                        .env("CRATONVM_TIER_C2_THRESHOLD", "600")
                        .env("CRATONVM_TIER_C2_MIN_INVOCATIONS", "500")
                        .env("CRATONVM_DBG_IR_COMPILES", "1")
                        // The VM helper's diagnostic latch uses the explicit
                        // flag; the compiler also accepts the `jitc` DBG tag.
                        .env("CRATONVM_DBG_JITC", "1");
                    cmd.env(
                        "CRATONVM_JIT_IR_VH_READ_BINDS",
                        if ir_disabled { "0" } else { "1" },
                    );
                    cmd.env(
                        "CRATONVM_JIT_VH_PRIMITIVE_READ_CACHE",
                        if cache_disabled { "0" } else { "1" },
                    );
                }
                if !compact {
                    cmd.env("CRATONVM_COMPACT_REF_FIELDS", "0");
                }
                let done =
                    common::wait_draining(cmd.spawn().expect("probe"), Duration::from_secs(120));
                let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
                let stderr = String::from_utf8_lossy(&done.output.stderr);
                assert!(
                    !done.timed_out && done.output.status.success(),
                    "{mode}/{nojit}: {stdout}\n{stderr}"
                );
                assert_eq!(stdout, expected, "{mode}/{nojit}: {stderr}");
                if !nojit && probe == "ByteBufferViewReadsProbe" {
                    assert!(
                        stderr.contains("byte-buffer-view-read served"),
                        "{mode}: no checked byte-buffer view was served: {stderr}"
                    );
                    assert!(
                        stderr.lines().any(|line| line.contains(
                            "[cratonvm-jit-disasm] full/ir ByteBufferViewReadsProbe.read("
                        )),
                        "{mode}: byte-buffer views have no actual IR artifact: {stderr}"
                    );
                }
                if !nojit && probe == "ByteBufferViewWritesProbe" {
                    assert!(
                        stderr.contains("byte-buffer-view-write served"),
                        "{mode}: no checked byte-buffer view write was served: {stderr}"
                    );
                    assert_eq!(
                        stderr.contains("byte-buffer-view-write-direct served"),
                        !cache_disabled,
                        "{mode}/{compact}/{cache_disabled}: direct write service differs: {stderr}"
                    );
                    if !cache_disabled {
                        for slot in 0..6 {
                            assert!(
                                stderr.lines().any(|line|
                                    line.contains("byte-buffer-view-write-direct served")
                                        && line.contains(&format!("slot={slot}"))),
                                "{mode}/{compact}: direct writer slot {slot} was not served: {stderr}"
                            );
                        }
                        assert!(
                            stderr.lines().any(|line| line.contains(
                                "[cratonvm-jit-disasm] full/ir ByteBufferViewWritesProbe.write("
                            )),
                            "{mode}/{compact}: writer lacks actual IR artifact: {stderr}"
                        );
                    } else {
                        assert!(
                            !stderr
                                .lines()
                                .any(|line| line.contains("ir-varhandle-read BOUND set")),
                            "{mode}: the write helper off switch retained an IR bind: {stderr}"
                        );
                    }
                }
                if !nojit
                    && matches!(
                        probe,
                        "ByteBufferViewReadsProbe" | "ByteBufferViewWritesProbe"
                    )
                {
                    assert_eq!(stderr.contains("byte-buffer-view-compact-fields served"), compact,
                        "{mode}/{probe}/{compact}: compact field proof engagement differs: {stderr}");
                }
                if !nojit
                    && probe != "ByteBufferViewReadsProbe"
                    && probe != "ByteBufferViewWritesProbe"
                    && (byte_view || mode == "--jdk-only")
                {
                    let served = stderr
                        .lines()
                        .find(|line| line.contains("VarHandle read thin direct calls:"))
                        .and_then(|line| line.split("served=").nth(1))
                        .and_then(|tail| tail.split_whitespace().next())
                        .and_then(|n| n.parse::<u64>().ok())
                        .unwrap_or(0);
                    if probe == "OptimizingVarHandleReadsProbe" {
                        if cache_disabled || !compact {
                            assert!(!stderr.contains("primitive-varhandle-read cached"),
                                "{mode}/{compact}: withdrawn or legacy read entered the cache: {stderr}");
                        } else {
                            assert!(
                                stderr.contains("primitive-varhandle-read cached"),
                                "{mode}: the primitive read cache was never used: {stderr}"
                            );
                        }
                        if ir_disabled {
                            assert!(!stderr.contains("ir-varhandle-read BOUND"),
                                "{mode}: the optimizing read kill switch did not withdraw binds: {stderr}");
                        } else {
                            assert!(stderr.contains("ir-varhandle-read BOUND"),
                                "{mode}/{compact}: no optimizing VarHandle read was bound: {stderr}");
                            let mut callers =
                                vec!["fieldCaller".to_string(), "viewCaller".to_string()];
                            for carrier in ['Z', 'B', 'C', 'S', 'I', 'J', 'F', 'D'] {
                                for read_mode in ["get", "getOpaque", "getAcquire", "getVolatile"] {
                                    callers.push(format!("fieldCaller{carrier}{read_mode}"));
                                }
                            }
                            for carrier in ['S', 'C', 'I', 'J', 'F', 'D'] {
                                callers.push(format!("viewCaller{carrier}"));
                            }
                            for caller in callers {
                                assert!(stderr.contains(&format!(
                                    "optimizing backend produced a body for OptimizingVarHandleReadsProbe.{caller}")),
                                    "{mode}/{compact}: {caller} never received an optimizing body: {stderr}");
                            }
                        }
                    }
                    // A handful of boot-time field reads cannot prove that
                    // the byte-array fixture's hot view loads were served.
                    let minimum = if probe == "ByteArrayViewReadsProbe" {
                        1_000
                    } else {
                        0
                    };
                    assert!(
                        served > minimum,
                        "{mode}: the primitive VarHandle helper served {served} reads, expected more than {minimum}: {stderr}"
                    );
                }
            }
        }
    }
}
