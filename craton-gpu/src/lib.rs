//! craton-gpu — Java annotation classes for GPU offload directives.
//!
//! Annotations live in src/main/java/craton/gpu/ and are compiled by
//! build.rs to a jar at build time.

/// Absolute path to the compiled annotations jar, or empty string if
/// the build host did not have `javac` / `jar` available.
pub const ANNOTATIONS_JAR: &str = env!("CRATON_GPU_ANNOTATIONS_JAR");

/// Absolute path to the directory containing compiled annotation
/// class files. Always set even when the jar wasn't produced.
pub const ANNOTATIONS_DIR: &str = env!("CRATON_GPU_ANNOTATIONS_DIR");
