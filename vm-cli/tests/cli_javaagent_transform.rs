// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! End-to-end: a `-javaagent:` `ClassFileTransformer` is offered the classes
//! being defined, and the bytes it returns are the ones the VM defines.
//!
//! This is the regression test for
//! `java-agent-transformer-never-fires-and-attach-list-throws-FIXED-20260806.md`,
//! whose failure shape was the worst one available: `addTransformer` returned
//! normally, `isRetransformClassesSupported()` answered `true`, and the
//! transformer was then never called for any class. An agent has no way to
//! detect that from inside — so the test has to be from outside, and it has to
//! assert the *effect* of the rewrite rather than a counter the agent keeps.
//!
//! `XformAgent` swaps the eight-byte constant `ORIGINAL` for `MODIFIED` in
//! `XformTarget`'s constant pool. `XformTarget.main` prints that constant. So:
//!
//!   * "the transformer was never called"      -> stdout says `ORIGINAL`
//!   * "called, but the result was discarded"  -> stdout says `ORIGINAL`
//!   * both halves work                        -> stdout says `MODIFIED`
//!
//! and the agent's own `agent offered XformTarget` line distinguishes the first
//! two when it fails.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

/// Build a `-javaagent:`-able JAR: `Premain-Class` in the manifest plus the
/// agent's class files (including the anonymous `ClassFileTransformer`
/// subclass, without which `premain` dies on a `NoClassDefFoundError`).
fn build_agent_jar(dest_jar: &Path, premain_class: &str, class_files: &[&str]) {
    let f = std::fs::File::create(dest_jar)
        .unwrap_or_else(|e| panic!("create {}: {e}", dest_jar.display()));
    let mut zip = zip::ZipWriter::new(f);
    let opts: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
    let manifest = format!(
        "Manifest-Version: 1.0\r\n\
         Premain-Class: {premain_class}\r\n\
         Agent-Class: {premain_class}\r\n\
         Can-Retransform-Classes: true\r\n\r\n"
    );
    zip.write_all(manifest.as_bytes()).unwrap();

    for stem in class_files {
        let src = common::resources_dir().join(format!("{stem}.class"));
        let bytes = std::fs::read(&src).unwrap_or_else(|e| panic!("read {}: {e}", src.display()));
        zip.start_file(format!("{stem}.class"), opts).unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap();
}

fn run_with_agent() -> (String, String, std::process::ExitStatus) {
    let tmp = tempfile::tempdir().expect("create tempdir");
    common::stage_class(tmp.path(), "XformTarget");

    let jar = tmp.path().join("xform-agent.jar");
    // `XformAgent$1` is the anonymous transformer; the JAR needs it too.
    build_agent_jar(&jar, "XformAgent", &["XformAgent", "XformAgent$1"]);

    let mut cmd = common::cratonvm_cmd();
    cmd.arg(format!("-javaagent:{}", jar.display()))
        .arg("--classpath")
        .arg(tmp.path())
        .arg("XformTarget")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let out = cmd.output().expect("spawn cratonvm");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status,
    )
}

#[test]
fn javaagent_transformer_rewrite_reaches_the_defined_class() {
    let (stdout, stderr, status) = run_with_agent();

    assert!(
        status.success(),
        "cratonvm exited with {status:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains("agent premain inst=true"),
        "premain did not run with a live Instrumentation\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains("agent offered XformTarget"),
        "the transformer was never offered the application's main class -- \
         `addTransformer` accepted it and then did nothing, which is the exact \
         regression this test exists for\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        stdout.contains("MODIFIED"),
        "the transformer ran but the bytes it returned were not the ones defined\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        !stdout.contains("ORIGINAL"),
        "the ORIGINAL constant survived, so the pre-transform bytes were defined\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
}

/// The transformer's `loader` argument must be the class's defining loader, not
/// always `null`.
///
/// This is not a detail: JaCoCo, and most APM and tracing agents, skip
/// `loader == null` outright because that means a bootstrap class. An
/// implementation that passed `null` for everything would pass the test above
/// and still instrument nothing at all in the field.
#[test]
fn javaagent_transformer_receives_a_non_null_loader_for_a_classpath_class() {
    let (stdout, stderr, status) = run_with_agent();
    assert!(
        status.success(),
        "cratonvm exited with {status:?}\n{stderr}"
    );
    assert!(
        stdout.contains("agent offered XformTarget loaderNull=false"),
        "a classpath class was offered with a null ClassLoader; agents read that \
         as \"bootstrap, not mine\" and skip it\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
}
