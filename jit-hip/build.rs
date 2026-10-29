// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Re-export the fixture directory `jit-cuda`'s build script produced.
//!
//! `JIT_CUDA_FIXTURE_DIR` is set with `cargo:rustc-env=`, which applies
//! only to the crate that emitted it — so this crate's golden test
//! cannot see it. Rather than compile the `.java` fixtures a second
//! time (two copies that could disagree about which javac made them),
//! this locates the directory `jit-cuda` already built and re-exports
//! the name.
//!
//! The lookup walks up from `OUT_DIR` to the shared build root and
//! finds the one `gpu-fixtures` directory under it. That is
//! deliberately simple: the alternative is the `links =` metadata
//! channel `craton-gpu4j` uses, which would mean adding a `links` key
//! to `jit-cuda` and is a heavier change than a test fixture path
//! warrants.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    // OUT_DIR is <target>/<profile>/build/<pkg>-<hash>/out
    let build_root = out_dir.ancestors().nth(2).map(|p| p.to_path_buf());
    let mut found: Option<std::path::PathBuf> = None;
    if let Some(root) = build_root.clone() {
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let candidate = entry.path().join("out").join("gpu-fixtures");
                if candidate.is_dir() {
                    // Several stale build directories can exist; prefer
                    // the most recently written, which is the one the
                    // current `jit-cuda` build produced.
                    let newer = match (&found, candidate.metadata().and_then(|m| m.modified())) {
                        (None, Ok(_)) => true,
                        (Some(prev), Ok(t)) => prev
                            .metadata()
                            .and_then(|m| m.modified())
                            .map(|pt| t > pt)
                            .unwrap_or(true),
                        _ => false,
                    };
                    if newer {
                        found = Some(candidate);
                    }
                }
            }
        }
    }
    match found {
        Some(dir) => println!("cargo:rustc-env=JIT_CUDA_FIXTURE_DIR={}", dir.display()),
        None => {
            // JIT round 22 wave 6 (lane review): on a clean target this
            // script can run before `jit-cuda`'s, which compiles the
            // fixtures; with only `rerun-if-changed=build.rs` the empty answer
            // was then cached for good and the golden tests failed until
            // build.rs was touched. Watching the build root reruns this once
            // `jit-cuda`'s build directory appears there.
            if let Some(root) = &build_root {
                println!("cargo:rerun-if-changed={}", root.display());
            }
            // Not fatal: the golden test asserts the set is non-empty
            // and will say so plainly, which is a better failure than a
            // build script that cannot explain itself.
            println!(
                "cargo:warning=jit-hip: no gpu-fixtures directory found; \
                 build cratonvm-jit-cuda first or its golden tests will be empty"
            );
            println!("cargo:rustc-env=JIT_CUDA_FIXTURE_DIR=");
        }
    }
}
