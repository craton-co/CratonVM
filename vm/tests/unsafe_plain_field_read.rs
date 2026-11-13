// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A compiled `Unsafe.getInt` / `getLong` of an INSTANCE field is answered by
//! the thin helper, not dispatched to the native.
//!
//! The helper served only primitive arrays and off-heap addresses, so every
//! read of a field -- Netty's `refCnt` check on every `ByteBuf` access -- paid
//! the registered native's whole funnel, about 600 ns, and
//! `AdvancedLeakAwareCompositeByteBufTest`'s 10-thread tests ran 100x slower
//! than HotSpot and timed out at 30 s under load. See
//! `docs/feature-designs/collections-interception.md`.
//!
//! The probe prints HotSpot's output in both modes, and the JIT method-stats census must
//! report the reads as served: `declined` stays a small fraction of `served`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

// each holder: 1000 * 2000 * (6 + 0x08) = 28_000_000
const EXPECTED: &str =
    "cold=28000000 hot=28000000\nafter=9 1122334455667788\nfloat=true\nPLAIN-FIELD-READ-OK\n";

/// `served=<n> declined=<m>` from the method-stats census Unsafe accessor line.
fn served_declined(stderr: &str) -> Option<(u64, u64)> {
    let line = stderr
        .lines()
        .find(|l| l.contains("JIT Unsafe accessor binds"))?;
    let num = |key: &str| -> Option<u64> {
        let rest = &line[line.find(key)? + key.len()..];
        rest.split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    };
    Some((num("served=")?, num("declined=")?))
}

#[test]
fn a_plain_unsafe_read_of_an_instance_field_is_served_by_the_helper() {
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
        .join("tests/unsafe_plain_field_read_fixtures/UnsafePlainFieldReadProbe.java");
    let compiled =
        Command::new(
            home.join("bin")
                .join(if cfg!(windows) { "javac.exe" } else { "javac" }),
        )
        .args(["-d"])
        .arg(dir.path())
        .arg(source)
        .output()
        .expect("launch javac");
    assert!(
        compiled.status.success(),
        "the probe failed to compile -- fix the fixture source. javac stderr:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for mode in ["--real-jdk", "--compatible"] {
        let mut cmd = Command::new(&binary);
        cmd.arg(mode)
            .arg("--java-home")
            .arg(&home)
            .arg("--sun-misc-unsafe-memory-access=allow")
            .arg("-cp")
            .arg(dir.path())
            .arg("UnsafePlainFieldReadProbe")
            .env("CRATONVM_DBG_JIT_METHOD_STATS", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let done =
            common::wait_draining(cmd.spawn().expect("launch probe"), Duration::from_secs(180));
        assert!(!done.timed_out, "probe timed out: {mode}");
        let stdout = String::from_utf8_lossy(&done.output.stdout).replace("\r\n", "\n");
        let stderr = String::from_utf8_lossy(&done.output.stderr);
        assert!(
            done.output.status.success(),
            "{mode}: the probe exited with {:?}:\n{stdout}\n{stderr}",
            done.output.status
        );
        assert!(
            stdout == EXPECTED,
            "{mode}: stdout differs from HotSpot's:\n{stdout}\n{stderr}"
        );
        let (_served, declined) = served_declined(&stderr)
            .unwrap_or_else(|| panic!("{mode}: no Unsafe accessor census line:\n{stderr}"));
        // 8,000,000 reads. Compiled, they are inline (a published cell) or the
        // helper's; only the few before a method is compiled reach the native.
        assert!(
            declined <= 100_000,
            "{mode}: the field reads were dispatched to the native: served={_served} declined={declined}"
        );
    }
}
