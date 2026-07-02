// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! craton-gpu — Java annotation classes for GPU offload directives.
//!
//! The annotation `.java` sources are NOT shipped inside this crate.
//! They live in an external standalone Maven project (the
//! craton-gpu-java repo). At build time `build.rs` locates that source
//! tree - via the `$CRATON_GPU_JAVA_SRC` env override, a
//! `craton-gpu-java/src/main/java` checkout beside the CratonVM workspace,
//! or (on Windows) a `C:/craton/...` default install - then compiles it
//! with `javac` and packages it with `jar`. The build resets its generated
//! classes directory before source discovery and compilation, so stale
//! `.class` files cannot survive a missing source tree, missing `javac`, or
//! failed compile. When no source tree is present the build degrades to an
//! empty jar and a `cargo:warning=` (it never fails), so the two constants
//! below may point at an empty directory / be empty.
//!
//! The compiled paths are exposed two ways: as the `env!()` constants
//! below for this crate, and (via the `links` key in `Cargo.toml`) as
//! `cargo:annotations_{dir,jar}=` metadata that cargo forwards to
//! dependents' build scripts as `DEP_CRATON_GPU_ANNOTATIONS_*`.

/// Absolute path to the compiled annotations jar, or empty string if
/// the build host did not have `javac` / `jar` available.
pub const ANNOTATIONS_JAR: &str = env!("CRATON_GPU_ANNOTATIONS_JAR");

/// Absolute path to the directory containing compiled annotation
/// class files. Always set even when the jar wasn't produced.
pub const ANNOTATIONS_DIR: &str = env!("CRATON_GPU_ANNOTATIONS_DIR");
