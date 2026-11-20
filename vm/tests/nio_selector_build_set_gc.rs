// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression coverage for `build_set`'s cross-call GC-safety fix
//! (`native-io/src/nio_selector.rs`, 2026-07-07). The fixture drives many
//! `Selector.selectedKeys()`/`.keys()` cycles under `CRATONVM_GC_STRESS` so
//! the native `HashSet` construction (`HashSet.<init>` + repeated `Set.add`,
//! each capable of triggering a moving GC) is exercised many times per run —
//! before the fix, this reproduced as an all-zero-header `java/util/Set`
//! receiver + `NoSuchMethodError Object.add`, matching the Tomcat Tribes
//! `NioReceiver.listen()` crash shape found via the
//! `nio-native-side-table-stale-objectref` doc audit.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const FIXTURE: &str = "NioSelectorBuildSetGc";
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR has no parent")
        .to_path_buf()
}

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
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = workspace_root().join("target").join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn classpath_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(dir) = std::env::var("CRATONVM_TEST_CLASSES_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("resources"),
    );
    candidates.into_iter().find(|dir| {
        dir.join("cratonvm")
            .join(format!("{FIXTURE}.class"))
            .exists()
    })
}


