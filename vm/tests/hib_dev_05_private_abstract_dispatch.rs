// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! HIB-DEV-05 regression — reflective `Method.invoke` of a **private** method
//! declared in an **abstract** class must dispatch the EXACT resolved method,
//! never retarget to a subclass's same-name private method.
//!
//! Root cause (fixed in `native-builtins/src/lang_class.rs::native_method_invoke`):
//! the non-virtual path used `ctx.invoke` → `invoke_on_class_shared`, which
//! retargets a call whose declaring class is abstract/interface onto the
//! receiver's concrete class. For a private method that merely *lives* in an
//! abstract class this ran the subclass override. `ObjectStreamClass.invokeWriteObject`
//! relies on exactly this shape (`AbstractSharedSessionContract.writeObject` is
//! private, declared in an abstract class, invoked on a `SessionImpl` receiver),
//! so the SessionFactory UUID was never serialized → null factory on deser → NPE.
//!
//! This test compiles a tiny Java program that reproduces the shape and runs it
//! through the `cratonvm` CLI. It skips gracefully (reports `skip`) when `javac`
//! or the CLI binary is unavailable, so it never misattributes a missing
//! toolchain as a failure.

use std::path::{Path, PathBuf};
use std::process::Command;

const SOURCE: &str = r#"
import java.lang.reflect.*;
public class PrivAbstractDispatch {
    static abstract class Base {
        abstract void doWork();
        private String tag() { return "BASE"; }
    }
    static final class Sub extends Base {
        void doWork() {}
        private String tag() { return "SUB"; }
    }
    public static void main(String[] a) throws Exception {
        Sub s = new Sub();
        Method m = Base.class.getDeclaredMethod("tag");
        m.setAccessible(true);
        System.out.println("TAG=" + m.invoke(s));
    }
}
"#;

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
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.parent().unwrap().join("target");
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = target.join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn javac_available() -> bool {
    Command::new("javac")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn private_method_in_abstract_class_is_not_retargeted() {
    if !javac_available() {
        eprintln!("[hib-dev-05] javac unavailable; skipping");
        return;
    }
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!("[hib-dev-05] cratonvm binary not found; build with `cargo build --release -p cratonvm-cli`");
            return;
        }
    };

    // Stage the source + compile under target/ so we never write into the source tree.
    let out = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target")
        .join("hib_dev_05_dispatch");
    let _ = std::fs::create_dir_all(&out);
    let src = out.join("PrivAbstractDispatch.java");
    if std::fs::write(&src, SOURCE).is_err() {
        eprintln!("[hib-dev-05] could not stage source; skipping");
        return;
    }
    let compiled = Command::new("javac")
        .arg("--release")
        .arg("17")
        .arg("-d")
        .arg(&out)
        .arg(&src)
        .output();
    match compiled {
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!("[hib_dev_05] javac could not be executed: {e}; skipping");
            return;
        }
        // javac RAN and rejected the source: the probe is broken, and skipping
        // here would make this test a permanent vacuous pass.
        Ok(o) => assert!(
            o.status.success(),
            "[hib_dev_05] the embedded probe failed to compile — fix the probe source. \
             javac stderr:\n{}",
            String::from_utf8_lossy(&o.stderr)
        ),
    }

    let mut cmd = Command::new(&bin);
    // Use a real boot JDK when one is on PATH so the real reflection path runs;
    // honor JAVA_HOME if set (matches how the suites invoke the binary).
    if let Ok(jh) = std::env::var("JAVA_HOME") {
        cmd.arg("--java-home").arg(jh);
    }
    let output = cmd
        .arg("--nojit")
        .arg("-cp")
        .arg(&out)
        .arg("PrivAbstractDispatch")
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[hib-dev-05] failed to spawn cratonvm: {e}; skipping");
            return;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("TAG=BASE"),
        "reflective invoke of a private method declared in an abstract class \
         must run the exact resolved method (Base.tag => \"BASE\"), not the \
         subclass override. Got:\n{stdout}"
    );
    assert!(
        !stdout.contains("TAG=SUB"),
        "private method was wrongly retargeted to the subclass override (HIB-DEV-05 regression)"
    );
}
