// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The launcher's build stamp: the `built on <time> with <compiler>` tail of
//! HotSpot's `Abstract_VM_Version::internal_vm_info_string()`, which this VM
//! publishes as `sun.rt.internalVersion` (`cratonvm_vm::set_build_stamp`).
//!
//! The time is `SOURCE_DATE_EPOCH` when it is set (the reproducible-builds
//! convention), else the commit time of the checkout's `HEAD`, so two builds of
//! one commit carry one stamp. HotSpot stamps its configure time; a commit
//! time is the reproducible figure of the same kind. No git and no
//! `SOURCE_DATE_EPOCH`: no stamp, and the string ends at the JRE version as
//! before. The compiler is `rustc <version>`, as HotSpot names `gcc <version>`.
//!
//! Only this crate reads the two variables, so a new commit relinks the
//! launcher and recompiles nothing else.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    // Phase 5 fast boot (`docs/jdk-only/boot-image-snapshot-20261028.md`): the
    // launcher is a position-dependent executable on Linux x86_64. As a PIE it
    // carried ~217k relative relocations, applied eagerly (BIND_NOW) on every
    // start -- ~1,100 extra page faults and most of a bare `--version`. A fixed
    // load address is also what lets a boot image's pointers into the binary
    // (fn pointers, vtables, static strings) stay valid without a re-exec.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64")
    {
        println!("cargo:rustc-link-arg-bin=cratonvm=-no-pie");
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .or_else(|| head_commit_time(Path::new(&manifest_dir)));
    let time = epoch.map(utc_timestamp).unwrap_or_default();
    let compiler = rustc_version().unwrap_or_default();
    println!("cargo:rustc-env=CRATONVM_BUILD_TIME={time}");
    println!("cargo:rustc-env=CRATONVM_BUILD_COMPILER={compiler}");
}

/// `git log -1 --format=%ct` of the checkout, and a rerun trigger on the files
/// that move when `HEAD` does (`HEAD` itself, the branch ref it names, and
/// `packed-refs`). `None` without git or outside a checkout.
fn head_commit_time(dir: &Path) -> Option<i64> {
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let time = git(&["log", "-1", "--format=%ct", "HEAD"])?.parse().ok()?;
    let mut watched = vec!["HEAD".to_string(), "packed-refs".to_string()];
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watched.push(reference);
    }
    for name in watched {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", &name]) {
            if Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
    Some(time)
}

/// `rustc <version>` of the compiler building this crate (`$RUSTC -V`'s first
/// two words).
fn rustc_version() -> Option<String> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let out = Command::new(rustc).arg("-V").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut words = text.split_whitespace();
    Some(format!("{} {}", words.next()?, words.next()?))
}

/// `epoch` seconds as HotSpot's `HOTSPOT_BUILD_TIME`: `YYYY-MM-DDTHH:MM:SSZ`,
/// UTC.
fn utc_timestamp(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}
