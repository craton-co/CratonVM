// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! VM-generated generic-lambda `ClassCastException`s must identify synthetic
//! map wrappers by their Java-visible concrete classes. Spring Boot's
//! `LambdaSafe` relies on that identity to ignore an erased-generic mismatch.

use std::path::{Path, PathBuf};
use std::process::Command;

const PROBE: &str = r#"
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.Map;

public class LambdaSafeUnmodifiableMapClassCastProbe {
  interface Processor<T> { T apply(T value); }

  @SuppressWarnings({ "unchecked", "rawtypes" })
  static Object safelyApply(Processor<?> processor, Object value) {
    try {
      return ((Processor) processor).apply(value);
    }
    catch (ClassCastException ex) {
      if (ex.getMessage().startsWith(value.getClass().getName())) {
        return value;
      }
      throw ex;
    }
  }

  static void check(String label, Map<?, ?> value) {
    Processor<String> uppercase = candidate -> candidate.toUpperCase();
    Object result = safelyApply(uppercase, value);
    if (result != value) {
      throw new AssertionError(label + " was not returned unchanged");
    }
    System.out.println(label + "=" + value.getClass().getName());
  }

  public static void main(String[] args) {
    check("empty", Map.of());
    check("one", Map.of("spring", "boot"));
    check("many", Map.of("spring", "boot", "craton", "vm"));
    check("unmodifiable", Collections.unmodifiableMap(
        new LinkedHashMap<>(Map.of("spring", "boot"))));
    System.out.println("OK");
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
    let source = classes.join("LambdaSafeUnmodifiableMapClassCastProbe.java");
    std::fs::write(&source, PROBE).expect("write probe source");
    let javac = jdk
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let status = Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(classes)
        .arg(&source)
        .status()
        .expect("run javac");
    assert!(status.success(), "javac failed");
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
        .arg("LambdaSafeUnmodifiableMapClassCastProbe")
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
fn lambda_safe_filters_all_unmodifiable_map_display_classes_in_jit_and_interpreter() {
    let Some(binary) = find_binary() else {
        eprintln!("[lambda_safe_unmodifiable_map_classcast] CRATONVM_BIN not set; skipping");
        return;
    };
    let Some(jdk) = find_jdk() else {
        eprintln!("[lambda_safe_unmodifiable_map_classcast] JDK not found; skipping");
        return;
    };
    let classes = std::env::temp_dir().join("cratonvm-lambda-safe-unmodifiable-map");
    compile_probe(&jdk, &classes);
    for nojit in [false, true] {
        let stdout = run_probe(&binary, &jdk, &classes, nojit);
        assert!(stdout.contains("empty=java.util.ImmutableCollections$MapN"));
        assert!(stdout.contains("one=java.util.ImmutableCollections$Map1"));
        assert!(stdout.contains("many=java.util.ImmutableCollections$MapN"));
        assert!(stdout.contains("unmodifiable=java.util.Collections$UnmodifiableMap"));
        assert!(stdout.contains("OK"), "unexpected output: {stdout}");
    }
}
