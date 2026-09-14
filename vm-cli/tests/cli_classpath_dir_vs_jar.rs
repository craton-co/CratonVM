// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Both classpath shapes work: HelloWorld can be loaded from a
//! directory entry on the classpath AND from a JAR entry on the
//! classpath.
//!
//! The directory path goes through `ClassPath::new` →
//! `DirectoryEntry`, the JAR path through `ClassPath::new` →
//! `JarEntry`. The two have completely independent class-bytes lookup
//! code paths in `classloading/src/class_path.rs`, so it's valuable to
//! cover both end-to-end from the CLI entry point.
//!
//! The JAR is built on-the-fly with the workspace-pinned `zip` crate;
//! it contains only `META-INF/MANIFEST.MF` (with `Main-Class:
//! HelloWorld`) and `HelloWorld.class`. Note that the test uses
//! `--classpath <jar>`, NOT `--jar <jar>`, so the manifest's
//! `Main-Class` is not consulted — we pass `HelloWorld` positionally.
//! That is intentional: this test isolates the classpath-shape axis
//! from the jar-launch-mode axis.

mod common;

use std::io::Read;

fn run_helloworld_with_classpath(
    cp: &std::path::Path,
) -> (std::process::ExitStatus, String, String) {
    let mut cmd = common::cratonvm_cmd();
    cmd.arg("--classpath")
        .arg(cp)
        .arg("HelloWorld")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("spawn cratonvm");
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .as_mut()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let status = child.wait().expect("wait for cratonvm");
    (status, stdout, stderr)
}

#[test]
fn helloworld_via_classpath_directory() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    common::stage_class(tmp.path(), "HelloWorld");

    let (status, stdout, stderr) = run_helloworld_with_classpath(tmp.path());
    assert!(
        status.success(),
        "directory classpath run failed: status={status:?}, stdout={stdout:?}, stderr={stderr:?}"
    );
    assert!(
        stdout.contains("Hello, World!"),
        "expected 'Hello, World!' in stdout, got {stdout:?}"
    );
}

#[test]
fn helloworld_via_classpath_jar() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    // Stage the .class first so we can read its bytes into the JAR.
    common::stage_class(tmp.path(), "HelloWorld");

    let class_path = tmp.path().join("HelloWorld.class");
    let jar_path = tmp.path().join("HelloWorld.jar");
    common::build_jar(&jar_path, "HelloWorld", &class_path);

    let (status, stdout, stderr) = run_helloworld_with_classpath(&jar_path);
    assert!(
        status.success(),
        "jar classpath run failed: status={status:?}, stdout={stdout:?}, stderr={stderr:?}"
    );
    assert!(
        stdout.contains("Hello, World!"),
        "expected 'Hello, World!' in stdout, got {stdout:?}"
    );
}
