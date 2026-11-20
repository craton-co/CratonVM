// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let dir = std::env::var("DEP_CRATONVM_GPU_FIXTURES_FIXTURES_DIR")
        .expect("jit-cuda must publish its compiled fixture directory");
    println!("cargo:rustc-env=JIT_CUDA_FIXTURE_DIR={dir}");
}
