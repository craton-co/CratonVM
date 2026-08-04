// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A lambda `REF_invokeSpecial` target must not be retargeted to the
//! receiver's runtime class.  This is the bytecode shape emitted for an
//! `Interface.super::method` reference.

use std::path::{Path, PathBuf};
use std::process::Command;

const PROBE: &str = r#"
import java.util.function.Supplier;

public class LambdaInvokeSpecialNoRetargetProbe {
  interface Parent {
    default String value() {
      return obtain(() -> defaultValue());
    }

    private String defaultValue() {
      return "parent";
    }

    private static String obtain(Supplier<String> fallback) {
      return fallback.get();
    }
  }

  static final class Child implements Parent {
    @Override
    public String value() {
      return obtain(Parent.super::value);
    }

    private static String obtain(Supplier<String> fallback) {
      return fallback.get();
    }
  }

  public static void main(String[] args) {
    String value = new Child().value();
    if (!"parent".equals(value)) {
      throw new AssertionError("expected parent, got " + value);
    }
    System.out.println("OK lambda-invokespecial-no-retarget=" + value);
  }
}
"#;

fn find_binary() -> Option<PathBuf> {
    std::env::var("CRATONVM_BIN")
        .ok()
        .map(PathBuf::from)
        .filter(|path| path.exists())
}

fn find_jdk() -> Option<PathBuf> {
    ["CRATONVM_TEST_JDK", "JAVA_HOME"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(PathBuf::from)
        .find(|path| path.exists())
        .or_else(|| {
            [
                "C:/Program Files/Java/jdk-25",
                "/home/victor/jdk25",
                "/data/data/jdk25-real",
            ]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.exists())
        })
}

fn compile_probe(jdk: &Path, classes: &Path) {
    std::fs::create_dir_all(classes).expect("create probe directory");
    let source = classes.join("LambdaInvokeSpecialNoRetargetProbe.java");
    std::fs::write(&source, PROBE).expect("write probe source");
    let javac = jdk
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let status = Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(classes)
        .arg(&source)
        .output()
        .expect("run javac");
    // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
    // means this javac is older than the level this probe compiles at, so it never
    // opened the file. That is a missing-toolchain condition — the same one the
    // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
    // source" sends the next reader to edit a correct `.java` file.
    //
    // Narrowly keyed on javac's own wording for an unsupported release, so a
    // genuine source error still reaches the assertion below and still fails loudly
    // (see `probe_compile_guard.rs` for why that must never become a skip).
    if !status.status.success() {
        let stderr_probe = String::from_utf8_lossy(&status.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[lambda_invokespecial_no_retarget] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return;
        }
    }
    assert!(
        status.status.success(),
        "[lambda_invokespecial_no_retarget] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
}

fn run_probe(binary: &Path, jdk: &Path, classes: &Path, nojit: bool) -> String {
    let mut command = Command::new(binary);
    command.arg("--java-home").arg(jdk);
    if nojit {
        command.arg("--nojit");
    }
    let output = command
        .arg("-cp")
        .arg(classes)
        .arg("LambdaInvokeSpecialNoRetargetProbe")
        .output()
        .expect("run CratonVM probe");
    assert!(
        output.status.success(),
        "probe failed (nojit={nojit}):\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn lambda_invokespecial_interface_super_is_not_retargeted_in_jit_or_interpreter() {
    let Some(binary) = find_binary() else {
        eprintln!("[lambda_invokespecial_no_retarget] CRATONVM_BIN not set; skipping");
        return;
    };
    let Some(jdk) = find_jdk() else {
        eprintln!("[lambda_invokespecial_no_retarget] JDK not found; skipping");
        return;
    };
    let classes = std::env::temp_dir().join("cratonvm-lambda-invokespecial-no-retarget");
    compile_probe(&jdk, &classes);
    for nojit in [false, true] {
        let stdout = run_probe(&binary, &jdk, &classes, nojit);
        assert!(
            stdout.contains("OK lambda-invokespecial-no-retarget=parent"),
            "unexpected output: {stdout}"
        );
    }
}
