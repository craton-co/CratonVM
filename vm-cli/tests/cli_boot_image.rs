// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Phase 5 boot image (WP5/WP6), end to end in child processes
//! (`docs/jdk-only/boot-image-snapshot-20261028.md`):
//!
//! * a capture run (`CRATONVM_BOOT_IMAGE=auto`, empty cache) and the restored
//!   run after it print exactly what a cold run prints (stdout, stderr, exit
//!   code), in both `--jdk-only` and `--compatible`;
//! * the restored run really restored (a separate `auto,verbose` run says so);
//! * a key mismatch (a boot-relevant environment variable changed) cold boots
//!   with identical output;
//! * a corrupted or truncated image is refused and cold boots with identical
//!   output.
//!
//! Linux x86_64 only (the only target with a restore path), and only where the
//! launcher finds a real JDK: without one the run is not eligible and every
//! arm is a cold boot, which the test reports and skips.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

mod common;

use std::path::Path;
use std::process::Command;

struct Run {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

fn run(mode: &str, cp: &Path, image: Option<(&str, &Path)>, extra_env: &[(&str, &str)]) -> Run {
    let mut cmd: Command = common::cratonvm_cmd();
    cmd.env_remove("CRATONVM_BOOT_IMAGE").env_remove("CRATONVM_BOOT_IMAGE_DIR");
    if let Some((knob, dir)) = image {
        cmd.env("CRATONVM_BOOT_IMAGE", knob).env("CRATONVM_BOOT_IMAGE_DIR", dir);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.arg(format!("--{mode}")).arg("-cp").arg(cp).arg("HelloWorld");
    let out = cmd.output().expect("spawn cratonvm");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code(),
    }
}

fn assert_same(what: &str, cold: &Run, other: &Run) {
    assert_eq!(cold.code, other.code, "{what}: exit code differs (stderr: {})", other.stderr);
    assert_eq!(cold.stdout, other.stdout, "{what}: stdout differs");
    assert_eq!(cold.stderr, other.stderr, "{what}: stderr differs");
}

fn images(dir: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "img"))
                .collect()
        })
        .unwrap_or_default()
}

/// The capture knob these tests use to obtain an image. The dangling-pointer
/// gate still refuses every capture today (pre-P0 allocator and C-heap
/// pointers, `docs/jdk-only/boot-image-snapshot-20261028.md`), so the restore
/// mechanics are exercised with its diagnosis override;
/// `default_auto_refuses_and_runs_cold` covers the shipped behaviour.
const CAPTURE: &str = "auto,unsafe-ignore-pointers";

/// `None` when the launcher does not use images here (no real JDK): the
/// verbose capture run says why.
fn capture(mode: &str, cp: &Path, dir: &Path) -> Option<String> {
    let v = run(mode, cp, Some((&format!("{CAPTURE},verbose"), dir)), &[]);
    // A refused capture is a failure, not a skip: it is how an unclassified
    // static, a thread alive at P1 or a dangling pointer shows up (WP7).
    assert!(
        !v.stderr.contains("boot image: capture refused"),
        "{mode}: capture refused: {}",
        v.stderr
    );
    if images(dir).is_empty() {
        eprintln!("skipping: no image captured ({mode}); launcher said: {}", v.stderr);
        return None;
    }
    Some(v.stderr)
}

fn round_trip(mode: &str) {
    let cp = tempfile::tempdir().unwrap();
    common::stage_class(cp.path(), "HelloWorld");
    let dir = tempfile::tempdir().unwrap();
    let cold = run(mode, cp.path(), None, &[]);
    assert_eq!(cold.code, Some(0), "cold run failed: {}", cold.stderr);

    // The capture run itself (no verbose: its output is compared).
    let cap = run(mode, cp.path(), Some((CAPTURE, dir.path())), &[]);
    assert_same("capture run", &cold, &cap);
    if images(dir.path()).is_empty() && capture(mode, cp.path(), dir.path()).is_none() {
        return;
    }
    let restored = run(mode, cp.path(), Some(("auto", dir.path())), &[]);
    assert_same("restored run", &cold, &restored);
    let proof = run(mode, cp.path(), Some(("auto,verbose", dir.path())), &[]);
    assert!(
        proof.stderr.contains("boot image: restored "),
        "the image was not used: {}",
        proof.stderr
    );
    assert_eq!(proof.stdout, cold.stdout);
}

#[test]
fn capture_and_restore_print_what_a_cold_boot_prints_jdk_only() {
    round_trip("jdk-only");
}

#[test]
fn capture_and_restore_print_what_a_cold_boot_prints_compatible() {
    round_trip("compatible");
}

#[test]
fn a_key_mismatch_cold_boots_with_identical_output() {
    let cp = tempfile::tempdir().unwrap();
    common::stage_class(cp.path(), "HelloWorld");
    let dir = tempfile::tempdir().unwrap();
    if capture("compatible", cp.path(), dir.path()).is_none() {
        return;
    }
    // `LC_*` is a key input: same command line, different key.
    let env = [("LC_CRATONVM_BOOT_IMAGE_TEST", "1")];
    let cold = run("compatible", cp.path(), None, &env);
    let v = run("compatible", cp.path(), Some(("auto,verbose", dir.path())), &env);
    assert!(!v.stderr.contains("boot image: restored "), "{}", v.stderr);
    let quiet = run("compatible", cp.path(), Some(("auto", dir.path())), &env);
    assert_same("key mismatch", &cold, &quiet);
}

fn corrupt_and_check(damage: impl Fn(&Path)) {
    let cp = tempfile::tempdir().unwrap();
    common::stage_class(cp.path(), "HelloWorld");
    let dir = tempfile::tempdir().unwrap();
    if capture("compatible", cp.path(), dir.path()).is_none() {
        return;
    }
    let img = images(dir.path()).remove(0);
    damage(&img);
    let damaged = std::fs::read(&img).unwrap();
    let cold = run("compatible", cp.path(), None, &[]);
    // A refused image is followed by a capture on the same run, which may
    // replace it: put the damaged bytes back before each run.
    let v = run("compatible", cp.path(), Some(("auto,verbose", dir.path())), &[]);
    assert!(v.stderr.contains("boot image: refused"), "{}", v.stderr);
    assert_eq!(v.stdout, cold.stdout);
    std::fs::write(&img, &damaged).unwrap();
    let quiet = run("compatible", cp.path(), Some(("auto", dir.path())), &[]);
    assert_same("damaged image", &cold, &quiet);
}

#[test]
fn default_auto_refuses_and_runs_cold() {
    // Without the diagnosis override, today's capture is refused by the
    // dangling-pointer gate: no image, and every run prints what a cold run
    // prints.
    let cp = tempfile::tempdir().unwrap();
    common::stage_class(cp.path(), "HelloWorld");
    let dir = tempfile::tempdir().unwrap();
    let cold = run("compatible", cp.path(), None, &[]);
    for _ in 0..2 {
        let r = run("compatible", cp.path(), Some(("auto", dir.path())), &[]);
        assert_same("auto run", &cold, &r);
    }
    let v = run("compatible", cp.path(), Some(("auto,verbose", dir.path())), &[]);
    assert_eq!(v.stdout, cold.stdout);
    if !images(dir.path()).is_empty() {
        // The gate passed (the pointer hazards were fixed): the image must
        // then restore correctly, which `round_trip` checks.
        assert!(v.stderr.contains("boot image: restored "), "{}", v.stderr);
    }
}

#[test]
fn a_corrupted_image_is_refused_and_cold_boots() {
    corrupt_and_check(|img| {
        let mut b = std::fs::read(img).unwrap();
        // Inside the checksummed metadata (the key blob follows the header).
        b[200] ^= 0x5A;
        std::fs::write(img, b).unwrap();
    });
}

#[test]
fn a_truncated_image_is_refused_and_cold_boots() {
    corrupt_and_check(|img| {
        let len = std::fs::metadata(img).unwrap().len();
        let f = std::fs::OpenOptions::new().write(true).open(img).unwrap();
        f.set_len(len / 2).unwrap();
    });
}
