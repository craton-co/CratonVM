// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Ratchet on who may leave the GLOBAL resolution counter alone
//! (`CRATONVM_SITE_GLOBAL_RESOLUTION_STAMP`, t1 wave 10 lane L71, audited by
//! t1 INTV on 2026-11-02; page
//! `t1-intv-proposal-loader-memo-rows-retire-boot-class-site-caches`).
//!
//! A site-cache entry stamped in the global-resolution mode (a referencing
//! class with no loader namespace) is retired by every resolution bump EXCEPT
//! a new row of a user loader's initiating memo or record. Its safety rests on
//! one structural fact: the only code that moves the ordinary resolution
//! counter without the global one is
//! `constants::bump_resolution_epoch_for_loader_row_in`
//! (`StoreEpochs::bump_resolution_epoch_loader_row_only`), and the only
//! callers of that are the two writers of those rows. A third caller would
//! keep stale global-mode entries valid across an event they must not
//! survive, with no exception and no failing test, so this file fails the
//! build instead and names the rule a reviewer has to check:
//!
//! 1. `bump_resolution_epoch_loader_row_only(` is called from
//!    `vm/src/runtime/interpreter/constants.rs` only (once), and defined in
//!    `classloading/src/class_manager.rs`.
//! 2. `bump_resolution_epoch_for_loader_row_in(` is called from
//!    `runtime/interpreter/constants.rs` (`cache_loader_initiated`),
//!    `runtime/resolve/initiating_records.rs` (the initiating-record writers)
//!    and the unit tests of `runtime/interpreter/site_cache.rs` only.
//! 3. `resolution_epoch_global_at(` is read by `site_cache.rs` (the stamp) and
//!    defined in `constants.rs` only.
//!
//! To add a caller: show that the new event changes nothing a class with NO
//! loader namespace can resolve (see `SiteCache::epochs_for_referencing`),
//! then add the file here.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|s| s.to_str()) == Some("tests") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name != "tests.rs" && !name.ends_with("_tests.rs") {
                out.push(path);
            }
        }
    }
}

/// `(path relative to `root`, 1-based line, line)` of every code line under
/// `root` that mentions `needle` and is neither a comment nor an item
/// definition (`fn needle`).
fn mentions(root: &Path, needle: &str) -> Vec<(String, usize, String)> {
    let mut files = Vec::new();
    rust_files(root, &mut files);
    assert!(
        !files.is_empty(),
        "found no Rust sources under {}; the scan would pass vacuously",
        root.display()
    );
    files.sort();
    let mut found = Vec::new();
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let shown = path
            .strip_prefix(root)
            .unwrap_or(path.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        for (i, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || !line.contains(needle) {
                continue;
            }
            if trimmed.contains(&format!("fn {needle}")) {
                continue;
            }
            found.push((shown.clone(), i + 1, trimmed.to_string()));
        }
    }
    found
}

fn offenders(found: &[(String, usize, String)], allowed_files: &[&str]) -> Vec<String> {
    found
        .iter()
        .filter(|(file, _, _)| !allowed_files.contains(&file.as_str()))
        .map(|(file, line, text)| format!("  {file}:{line}: {text}"))
        .collect()
}

fn vm_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn classloading_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("classloading")
        .join("src")
}

#[test]
fn t1_intv_only_the_loader_row_bump_skips_the_global_counter() {
    // The single caller in the vm crate.
    let in_vm = mentions(&vm_src(), "bump_resolution_epoch_loader_row_only(");
    let bad = offenders(&in_vm, &["runtime/interpreter/constants.rs"]);
    assert!(
        bad.is_empty(),
        "`StoreEpochs::bump_resolution_epoch_loader_row_only` moves the ordinary resolution \
         counter and NOT the global one, so a site entry stamped in the global-resolution mode \
         (`CRATONVM_SITE_GLOBAL_RESOLUTION_STAMP`) survives it. Only a new row of a user \
         loader's initiating memo or record may do that, through \
         `constants::bump_resolution_epoch_for_loader_row_in`. Offenders:\n{}",
        bad.join("\n")
    );
    assert_eq!(
        in_vm.len(),
        1,
        "expected exactly one call (bump_resolution_epoch_for_loader_row_in), found {in_vm:?}"
    );
    // Nothing else in classloading calls it outside the StoreEpochs file.
    let in_classloading = mentions(
        &classloading_src(),
        "bump_resolution_epoch_loader_row_only(",
    );
    let bad = offenders(&in_classloading, &["class_manager.rs"]);
    assert!(bad.is_empty(), "offenders:\n{}", bad.join("\n"));
}

#[test]
fn t1_intv_only_the_two_loader_row_writers_call_the_loader_row_bump() {
    let callers = mentions(&vm_src(), "bump_resolution_epoch_for_loader_row_in(");
    let bad = offenders(
        &callers,
        &[
            "runtime/interpreter/constants.rs",
            "runtime/resolve/initiating_records.rs",
            "runtime/interpreter/site_cache.rs",
        ],
    );
    assert!(
        bad.is_empty(),
        "`bump_resolution_epoch_for_loader_row_in` leaves the global resolution counter alone: \
         only the writers of an initiating-memo / initiating-record row may call it (a row is \
         read by loader-namespaced resolutions only). Offenders:\n{}",
        bad.join("\n")
    );
    // The writers themselves are still wired to it: a row writer that went
    // back to the full bump would silently disable the optimisation.
    assert!(
        callers
            .iter()
            .any(|(f, _, _)| f == "runtime/interpreter/constants.rs"),
        "`cache_loader_initiated` no longer calls the loader-row bump"
    );
    assert!(
        callers
            .iter()
            .any(|(f, _, _)| f == "runtime/resolve/initiating_records.rs"),
        "the initiating-record writers no longer call the loader-row bump"
    );
}

#[test]
fn t1_intv_only_the_site_stamp_reads_the_global_counter() {
    let readers = mentions(&vm_src(), "resolution_epoch_global_at(");
    let bad = offenders(
        &readers,
        &[
            "runtime/interpreter/site_cache.rs",
            "runtime/interpreter/constants.rs",
        ],
    );
    assert!(
        bad.is_empty(),
        "the global resolution counter does not move on a user loader's new initiating row, so \
         only an entry whose referencing class has no loader namespace may be validated against \
         it (`SiteCache::epochs_for_referencing`). Offenders:\n{}",
        bad.join("\n")
    );
}
