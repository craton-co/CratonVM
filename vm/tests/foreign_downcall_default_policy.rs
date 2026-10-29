// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Netty's Java 24 cleaner can bind malloc/free after a restricted-method
//! warning. A second gate at invocation must not reject that accepted handle.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
mod common;

#[test]
fn foreign_downcalls_follow_jdk25_allow_warn_deny_in_both_modes_and_tiers() {
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("target")
        });
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
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/foreign_downcall_default_policy_fixtures/ForeignDowncallPolicyProbe.java");
    let compiled =
        Command::new(
            home.join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" }),
        )
        .arg("-d")
        .arg(dir.path())
        .arg(&source)
        .arg(source.with_file_name("NamedDowncallPolicyLauncher.java"))
        .output()
        .expect("launch javac");
    assert!(
        compiled.status.success(),
        "the probe failed to compile -- fix the fixture source. javac stderr:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let named_source = dir.path().join("named-source");
    let named_classes = dir.path().join("named-classes");
    std::fs::create_dir_all(&named_source).unwrap();
    let named_probe = named_source.join("ForeignDowncallPolicyProbe.java");
    std::fs::write(
        &named_probe,
        format!(
            "package ffm.probe;\n{}",
            std::fs::read_to_string(&source).unwrap()
        ),
    )
    .unwrap();
    let module_info = named_source.join("module-info.java");
    std::fs::write(&module_info, "module ffm.probe { exports ffm.probe; }\n").unwrap();
    let compiled =
        Command::new(
            home.join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" }),
        )
        .arg("-d")
        .arg(&named_classes)
        .arg(&named_probe)
        .arg(&module_info)
        .output()
        .expect("compile named probe");
    assert!(
        compiled.status.success(),
        "the probe failed to compile -- fix the fixture source. javac stderr:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for mode in ["--jdk-only", "--compatible"] {
        for nojit in [false, true] {
            for (policy, granted, before, after, denied) in [
                ("warn", false, false, true, false),
                ("allow", false, false, false, false),
                ("deny", false, false, false, true),
                ("deny", true, true, true, false),
            ] {
                let mut cmd = Command::new(&binary);
                cmd.arg(mode)
                    .arg(format!("--illegal-native-access={policy}"));
                if granted {
                    cmd.arg("--enable-native-access=ALL-UNNAMED");
                }
                if nojit {
                    cmd.arg("--nojit");
                }
                cmd.arg("--java-home")
                    .arg(&home)
                    .arg("-cp")
                    .arg(dir.path())
                    .arg("ForeignDowncallPolicyProbe")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let done = common::wait_draining(
                    cmd.spawn().expect("launch probe"),
                    Duration::from_secs(120),
                );
                let stdout = String::from_utf8_lossy(&done.output.stdout);
                let stderr = String::from_utf8_lossy(&done.output.stderr);
                assert!(
                    !done.timed_out && done.output.status.success(),
                    "{mode}/{nojit}/{policy}/{granted}: {stdout}\n{stderr}"
                );
                assert!(
                    stdout.contains(&format!("nativeAccess.before={before}")),
                    "{stdout}\n{stderr}"
                );
                assert_eq!(
                    stderr.matches("Linker::downcallHandle has been called by").count(),
                    usize::from(policy == "warn" && !granted),
                    "{mode}/{nojit}/{policy}/{granted}: warnings must occur once at linking, not at invocation: {stderr}"
                );
                assert!(
                    !stderr.contains("MemorySegment::"),
                    "spurious restricted-method warning: {stderr}"
                );
                if denied {
                    assert!(
                        stdout.contains("LINK-DENIED") && !stdout.contains("nativeAccess.after="),
                        "{stdout}\n{stderr}"
                    );
                } else {
                    assert!(
                        stdout.contains(&format!("nativeAccess.after={after}"))
                            && stdout.contains("FOREIGN-DOWNCALL-OK"),
                        "{mode}/{nojit}/{policy}/{granted}: {stdout}\n{stderr}"
                    );
                }
            }
            // ALL-UNNAMED must leave a named caller denied. Compare against
            // HotSpot rather than relying only on the policy registry's state.
            for grant in [None, Some("ALL-UNNAMED"), Some("ffm.probe")] {
                let flags = |cmd: &mut Command| {
                    cmd.arg("--illegal-native-access=deny");
                    if let Some(grant) = grant {
                        cmd.arg(format!("--enable-native-access={grant}"));
                    }
                    cmd.arg("--module-path")
                        .arg(&named_classes)
                        .arg("--add-modules=ffm.probe")
                        .arg("-cp")
                        .arg(dir.path())
                        .arg("NamedDowncallPolicyLauncher")
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped());
                };
                let mut oracle = Command::new(home.join("bin").join(if cfg!(windows) {
                    "java.exe"
                } else {
                    "java"
                }));
                flags(&mut oracle);
                let oracle = common::wait_draining(
                    oracle.spawn().expect("HotSpot named probe"),
                    Duration::from_secs(120),
                );
                assert!(
                    !oracle.timed_out && oracle.output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&oracle.output.stderr)
                );
                let mut cmd = Command::new(&binary);
                cmd.arg(mode).arg("--java-home").arg(&home);
                if nojit {
                    cmd.arg("--nojit");
                }
                flags(&mut cmd);
                let done = common::wait_draining(
                    cmd.spawn().expect("named probe"),
                    Duration::from_secs(120),
                );
                let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
                let stderr = String::from_utf8_lossy(&done.output.stderr);
                assert!(
                    !done.timed_out && done.output.status.success(),
                    "{mode}/{nojit}/{grant:?}: {stdout}\n{stderr}"
                );
                assert_eq!(
                    stdout,
                    String::from_utf8_lossy(&oracle.output.stdout).replace("\r\n", "\n"),
                    "{mode}/{nojit}/{grant:?}: {stderr}"
                );
            }
        }
    }
}
