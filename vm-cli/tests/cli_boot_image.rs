// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Phase 5 boot image (WP5/WP6), end to end in child processes
//! (`docs/feature-designs/real-jdk-mode.md`):
//!
//! * a capture run (`CRATONVM_BOOT_IMAGE=auto`, empty cache) and the restored
//!   run after it print exactly what a cold run prints (stdout, stderr, exit
//!   code), in both `--real-jdk` and `--fast-boot`;
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
    // No image arm = a cold baseline: `--compatible` is on by default, so say off.
    cmd.env("CRATONVM_BOOT_IMAGE", "off").env_remove("CRATONVM_BOOT_IMAGE_DIR");
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

/// The knob these tests use to obtain an image: the first `auto` run captures.
const CAPTURE: &str = "auto";

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
fn auto_captures_on_the_first_run_and_restores_on_the_next() {
    let cp = tempfile::tempdir().unwrap();
    common::stage_class(cp.path(), "HelloWorld");
    let dir = tempfile::tempdir().unwrap();
    let cold = run("compatible", cp.path(), None, &[]);
    let first = run("compatible", cp.path(), Some(("auto,verbose", dir.path())), &[]);
    if images(dir.path()).is_empty() {
        eprintln!("skipping: no image captured; launcher said: {}", first.stderr);
        return;
    }
    assert!(first.stderr.contains("boot image: captured "), "{}", first.stderr);
    assert_eq!(first.stdout, cold.stdout);
    let second = run("compatible", cp.path(), Some(("auto,verbose", dir.path())), &[]);
    assert!(second.stderr.contains("boot image: restored "), "{}", second.stderr);
    assert_eq!(second.stdout, cold.stdout);
    // The first restored run left the list of pages it copied; a later restore
    // prefaults them, and a damaged list is ignored.
    let cow = images(dir.path()).remove(0).with_extension("cow");
    assert!(cow.exists(), "a restored run records its copy-on-write pages");
    let third = run("compatible", cp.path(), Some(("auto", dir.path())), &[]);
    assert_same("prefaulted restore", &cold, &third);
    std::fs::write(&cow, [0xffu8; 40]).unwrap();
    let fourth = run("compatible", cp.path(), Some(("auto", dir.path())), &[]);
    assert_same("restore with a damaged page list", &cold, &fourth);
}

#[test]
fn compatible_is_on_by_default_and_captures_on_the_second_run_of_a_command_line() {
    let cp = tempfile::tempdir().unwrap();
    common::stage_class(cp.path(), "HelloWorld");
    let dir = tempfile::tempdir().unwrap();
    let cold = run("compatible", cp.path(), None, &[]);
    // No `CRATONVM_BOOT_IMAGE`: only the cache directory is set.
    let env = [("CRATONVM_BOOT_IMAGE_DIR", dir.path().to_str().unwrap())];
    let implicit = |mode: &str| {
        let mut cmd = common::cratonvm_cmd();
        cmd.env_remove("CRATONVM_BOOT_IMAGE").env(env[0].0, env[0].1);
        cmd.arg(format!("--{mode}")).arg("-cp").arg(cp.path()).arg("HelloWorld");
        let o = cmd.output().expect("spawn cratonvm");
        (images(dir.path()).len(), String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let (n1, out1) = implicit("compatible");
    let (_, out2) = implicit("compatible");
    // The second run captures in a forked child and does not wait for it.
    let t = std::time::Instant::now();
    while images(dir.path()).is_empty() && t.elapsed().as_secs() < 30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let n2 = images(dir.path()).len();
    let (n3, out3) = implicit("compatible");
    if n2 == 0 {
        eprintln!("skipping: no image captured (no real JDK?)");
        return;
    }
    assert_eq!((n1, n2, n3), (0, 1, 1), "first run cold, second captures, third restores");
    assert!([out1, out2, out3].iter().all(|o| *o == cold.stdout));
    // The launcher default (jdk-only) is not on without the knob.
    let dir2 = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let mut cmd = common::cratonvm_cmd();
        cmd.env_remove("CRATONVM_BOOT_IMAGE").env("CRATONVM_BOOT_IMAGE_DIR", dir2.path());
        cmd.arg("-cp").arg(cp.path()).arg("HelloWorld");
        cmd.output().expect("spawn cratonvm");
    }
    assert!(images(dir2.path()).is_empty());
}

#[test]
fn a_rewritten_main_class_file_refuses_the_image() {
    // The image carries the launcher's main-class step, so the class file it
    // loaded is part of what it depends on: rewriting it in place (the
    // directory's mtime does not move) must send the next run to a cold boot.
    let cp = tempfile::tempdir().unwrap();
    common::stage_class(cp.path(), "HelloWorld");
    let dir = tempfile::tempdir().unwrap();
    let cold = run("compatible", cp.path(), None, &[]);
    let first = run("compatible", cp.path(), Some(("auto,verbose", dir.path())), &[]);
    if images(dir.path()).is_empty() {
        eprintln!("skipping: no image captured; launcher said: {}", first.stderr);
        return;
    }
    let ok = run("compatible", cp.path(), Some(("auto,verbose", dir.path())), &[]);
    assert!(ok.stderr.contains("boot image: restored "), "{}", ok.stderr);
    let class = cp.path().join("HelloWorld.class");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&class, std::fs::read(&class).unwrap()).unwrap();
    let after = run("compatible", cp.path(), Some(("auto,verbose", dir.path())), &[]);
    assert!(after.stderr.contains("class path file changed"), "{}", after.stderr);
    assert_eq!(after.stdout, cold.stdout);
}

#[test]
fn a_remembered_refusal_skips_the_capture_until_capture_is_asked_for() {
    let cp = tempfile::tempdir().unwrap();
    common::stage_class(cp.path(), "HelloWorld");
    let dir = tempfile::tempdir().unwrap();
    let first = run("compatible", cp.path(), Some(("auto,verbose", dir.path())), &[]);
    let Some(img) = images(dir.path()).pop() else {
        eprintln!("skipping: no image captured; launcher said: {}", first.stderr);
        return;
    };
    // What a refused capture leaves: no image, a marker beside where it goes.
    std::fs::remove_file(&img).unwrap();
    std::fs::write(img.with_extension("refused"), "pretend").unwrap();
    let v = run("compatible", cp.path(), Some(("auto,verbose", dir.path())), &[]);
    assert!(v.stderr.contains("an earlier capture was refused (pretend)"), "{}", v.stderr);
    assert!(images(dir.path()).is_empty(), "auto must not capture again");
    let forced = run("compatible", cp.path(), Some(("capture", dir.path())), &[]);
    assert_eq!(forced.code, Some(0), "{}", forced.stderr);
    assert_eq!(images(dir.path()).len(), 1, "`capture` retries");
    assert!(!img.with_extension("refused").exists(), "a good capture clears the marker");
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
