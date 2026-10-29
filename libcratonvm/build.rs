// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
//! Build script for `libcratonvm`.
//!
//! Its only job is the **opt-in** cbindgen header regeneration step for the
//! flat C API (`docs/feature-designs/embedding-api.md`, Layer 2 "wire real
//! cbindgen generation as a build step"). It is a **no-op by default** so that:
//!
//!   * normal builds add no dependency (cbindgen is NOT a Cargo dependency —
//!     the graph and `Cargo.lock` are untouched, so offline builds are unaffected),
//!   * the build never fails just because `cbindgen` is not installed.
//!
//! To regenerate `include/cratonvm.h`:
//!
//! ```sh
//! cargo install cbindgen          # once, if not already on PATH
//! CRATONVM_REGEN_HEADER=1 cargo build -p libcratonvm
//! ```
//!
//! With `CRATONVM_REGEN_HEADER` set, the script invokes the `cbindgen` CLI with
//! `cbindgen.toml` and overwrites the checked-in header. The checked-in header
//! is maintained by hand to stay ABI-compatible with `cbindgen.toml`; a
//! regeneration may still differ in comments or JNI-style field spelling.

use std::path::Path;
use std::process::Command;

fn main() {
    // Only re-run when the relevant inputs change; keep default builds cheap.
    println!("cargo:rerun-if-env-changed=CRATONVM_REGEN_HEADER");
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cbindgen.toml");

    if std::env::var_os("CRATONVM_REGEN_HEADER").is_none() {
        return; // default: do nothing.
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let crate_dir = Path::new(&manifest_dir);
    let out = crate_dir.join("include").join("cratonvm.h");

    // Invoke the cbindgen CLI: `cbindgen --config cbindgen.toml --output <out> <crate_dir>`.
    let status = Command::new("cbindgen")
        .arg("--config")
        .arg(crate_dir.join("cbindgen.toml"))
        .arg("--output")
        .arg(&out)
        .arg(crate_dir)
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("cargo:warning=libcratonvm: regenerated {}", out.display());
        }
        Ok(s) => {
            // Non-fatal: a failed regen must not break the build (the
            // hand-maintained header is still valid).
            println!("cargo:warning=libcratonvm: cbindgen exited with {s}; header left unchanged");
        }
        Err(e) => {
            println!(
                "cargo:warning=libcratonvm: cbindgen not runnable ({e}); \
                 install it with `cargo install cbindgen`. Header left unchanged."
            );
        }
    }
}
