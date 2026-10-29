// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A declared flag cannot be overridden by mutating `environ`.
//!
//! [`cratonvm_types::flags::flags`] serves every declared `CRATONVM_*` variable
//! from one process-wide snapshot latched on the first read of *any* flag. So
//! `std::env::set_var("CRATONVM_FOO", "1")` inside a test changes `environ` and
//! nothing the VM will read — unless that test happens to be the first thing in
//! its binary to touch a flag. The test then passes when it runs first and
//! silently exercises the developer's ambient environment when it does not,
//! which is an order-dependent test that presents as a flake. The full
//! diagnosis is in
//! `libcratonvm-no-jdk-test-order-dependent-fixed-20260730.md`.
//!
//! The supported replacements are `flags::with_thread_overrides` (default) and
//! `flags::with_process_overrides` (when the reader runs on a thread the test
//! did not create). This test fails the build if a new `set_var`/`remove_var`
//! of a declared name appears, because the defect is invisible in review — the
//! broken line is indistinguishable from the working one.
//!
//! Companion to `flag_surface.rs`, which pins *which* names are declared.
//!
//! Scope: this catches string-literal names only. A `set_var(key, …)` whose key
//! is a variable cannot be judged statically; the two helpers that used to do
//! that for declared flags (`libcratonvm`'s and `vm/src/config.rs`'s `with_env`)
//! now route declared names through the override hooks and pass only
//! undeclared names — `JAVA_HOME`, `JBOSS_HOME` — through to `environ`, which
//! is correct because undeclared names keep `std::env`'s live-read semantics.

use cratonvm_types::flag_groups::{Group, INVENTORY, SCALARS};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Directory names never scanned: build output, VCS metadata, and the Java
/// application harnesses under `apps/` (not Rust sources).
const SKIPPED_DIRS: &[&str] = &["target", ".git", "apps", "node_modules"];

/// Sites permitted to write a declared name into `environ`, with the reason.
///
/// Empty by design. `flag_groups::expand_process_env` — the one production
/// writer — builds its keys dynamically and so is out of this test's reach
/// anyway; it is also correct, because it runs in the single-threaded launcher
/// window *before* the snapshot latches. If an entry is ever added here it
/// needs a comment saying why the snapshot cannot serve that call site.
const ALLOWED: &[(&str, &str)] = &[];

fn declared() -> BTreeSet<&'static str> {
    INVENTORY
        .iter()
        .flat_map(|e| [e.on_key, e.off_key].into_iter().flatten())
        .chain(SCALARS.iter().copied())
        .chain(Group::ALL.iter().map(|g| g.var()))
        .collect()
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("types/ always has a workspace root above it")
        .to_path_buf()
}

fn rust_sources(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if !SKIPPED_DIRS.contains(&name) && !name.starts_with('.') {
                rust_sources(&path, out);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// The name literal passed as the first argument of a `set_var`/`remove_var`
/// call starting at `rest`, if it is a plain (non-raw, non-escaped) literal.
fn first_string_argument(rest: &str) -> Option<&str> {
    let open = rest.find('(')? + 1;
    let after = rest[open..].trim_start();
    let quoted = after.strip_prefix('"')?;
    let end = quoted.find('"')?;
    Some(&quoted[..end])
}

#[test]
fn no_test_overrides_a_declared_flag_through_the_environment() {
    let declared = declared();
    let root = workspace_root();
    let mut sources = Vec::new();
    rust_sources(&root, &mut sources);
    assert!(
        sources.len() > 100,
        "only found {} Rust sources under {} — the walk is not reaching the \
         workspace, so this guard would pass vacuously",
        sources.len(),
        root.display()
    );

    let this_file = Path::new(file!()).file_name().expect("test file name");
    let mut offenders = Vec::new();
    for path in &sources {
        if path.file_name() == Some(this_file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if !text.contains("set_var") && !text.contains("remove_var") {
            continue;
        }
        let relative = path.strip_prefix(&root).unwrap_or(path);
        let shown = relative.display().to_string().replace('\\', "/");
        for (index, line) in text.lines().enumerate() {
            for call in ["set_var", "remove_var"] {
                let Some(at) = line.find(call) else { continue };
                let Some(name) = first_string_argument(&line[at + call.len()..]) else {
                    continue;
                };
                if !declared.contains(name) {
                    continue;
                }
                let site = format!("{shown}:{}", index + 1);
                if ALLOWED.iter().any(|(allowed, _)| *allowed == site) {
                    continue;
                }
                offenders.push(format!("{site}: {call}(\"{name}\", …)"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "{} declared CratonVM flag(s) are being set through the process \
         environment. That does not work: `flags()` serves declared names from \
         a snapshot latched on first read, so this only takes effect when the \
         call wins the race to initialise it.\n\n\
         Use `cratonvm_types::flags::with_thread_overrides(&[(NAME, \
         Some(value))], || …)`, or `with_process_overrides` when the reader \
         runs on a thread the test did not create.\n\n  {}",
        offenders.len(),
        offenders.join("\n  ")
    );
}
