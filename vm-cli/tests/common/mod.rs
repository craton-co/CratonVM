// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Shared helpers for end-to-end vm-cli integration tests.
//
// Each `tests/cli_*.rs` binary pulls these in via `mod common;` and uses
// them to (a) stage pre-built `.class` fixtures from `tests/resources/`
// into a per-test temp directory, and (b) spawn the `cratonvm` binary
// built by Cargo for the current target via `env!("CARGO_BIN_EXE_*")`.
//
// The fixtures committed under `tests/resources/` are pre-compiled with
// `javac --release 21` so the tests do not depend on a system JDK being
// installed — only on the cratonvm bin Cargo just built.

#![allow(dead_code)] // Each test binary uses a different subset of helpers.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Absolute path to the directory holding committed `.class` fixtures.
///
/// `CARGO_MANIFEST_DIR` is set at compile time to the absolute path of
/// the crate root (vm-cli/). We just append `tests/resources`.
pub fn resources_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("resources")
}

/// Copy `<fixture_stem>.class` from `tests/resources/` into `dest_dir`.
///
/// Tests use this to construct a small per-test classpath directory:
/// build a `tempfile::tempdir()`, call `stage_class(tmp, "HelloWorld")`,
/// then pass `tmp.path()` as `--classpath` to the spawned binary.
pub fn stage_class(dest_dir: &Path, fixture_stem: &str) {
    let src = resources_dir().join(format!("{fixture_stem}.class"));
    let dst = dest_dir.join(format!("{fixture_stem}.class"));
    std::fs::copy(&src, &dst).unwrap_or_else(|e| {
        panic!(
            "failed to stage fixture {} -> {}: {e}",
            src.display(),
            dst.display()
        )
    });
}

/// Build a Command pointing at the `cratonvm` binary Cargo built for
/// this test invocation. `CARGO_BIN_EXE_cratonvm` is set by Cargo when
/// running integration tests for a crate whose `[[bin]]` named `cratonvm`
/// is in the same package.
pub fn cratonvm_cmd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cratonvm"))
}

/// Convenience: build a single-class JAR with `Main-Class: <main_class>`.
///
/// `class_file` is the path to a compiled `.class` whose simple name
/// (without `.class`) MUST equal `main_class`. The JAR is written to
/// `dest_jar`; any parent directory is assumed to already exist.
pub fn build_jar(dest_jar: &Path, main_class: &str, class_file: &Path) {
    use std::io::Write;
    let f = std::fs::File::create(dest_jar)
        .unwrap_or_else(|e| panic!("create {}: {e}", dest_jar.display()));
    let mut zip = zip::ZipWriter::new(f);
    let opts: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
    let manifest = format!("Manifest-Version: 1.0\r\nMain-Class: {main_class}\r\n\r\n");
    zip.write_all(manifest.as_bytes()).unwrap();

    let entry_name = format!("{main_class}.class");
    zip.start_file(&entry_name, opts).unwrap();
    let bytes =
        std::fs::read(class_file).unwrap_or_else(|e| panic!("read {}: {e}", class_file.display()));
    zip.write_all(&bytes).unwrap();

    zip.finish().unwrap();
}
