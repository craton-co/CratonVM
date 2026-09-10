// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! VM-generated generic-lambda `ClassCastException`s must identify synthetic
//! map wrappers by their Java-visible concrete classes. Spring Boot's
//! `LambdaSafe` relies on that identity to ignore an erased-generic mismatch.
//!
//! # The probe's rule is Spring's rule, and it was not
//!
//! `LambdaSafe.startsWithArgumentClassName` accepts TWO prefixes -- the bare
//! `argument.getClass().getName()`, and `"class " + name + " "`, which is the
//! wording HotSpot has used since JDK 11. This probe used to test only the
//! first, and that is a contract **no JVM honours**: measured 2026-09-08 on
//! Temurin 25.0.4, the same source fails on its FIRST call under HotSpot with
//!
//! ```text
//! class java.util.ImmutableCollections$MapN cannot be cast to class java.lang.String
//!     (java.util.ImmutableCollections$MapN and java.lang.String are in module
//!      java.base of loader 'bootstrap')
//! ```
//!
//! CratonVM passed it only because its message was the pre-JDK-11 bare form --
//! and then only sometimes: the HotSpot wording needs both operands' module and
//! loader, which `klass_origin` could name only for a class that happened to be
//! LOADED, so `Map.of()` (an internal stamp rendered as `MapN`) printed the bare
//! form while `Map.of(k,v,k,v)` printed the full one in the same run. Both
//! halves are fixed: the message no longer depends on load history (see
//! `runtime/exceptions.rs`), and the check below is Spring's own.
//!
//! What the file tests is unchanged and is the thing its title names: whichever
//! wording carries it, the class in the message is the Java-visible concrete
//! class (`java.util.ImmutableCollections$MapN`), never the private stamp.

use std::path::{Path, PathBuf};
use std::process::Command;

const PROBE: &str = r#"
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.Map;

public class LambdaSafeUnmodifiableMapClassCastProbe {
  interface Processor<T> { T apply(T value); }

  // The message the last refused cast carried, so the harness can assert that
  // it NAMES the Java-visible class rather than only that `getClass()` does.
  // Without this the file's title -- "must identify synthetic map wrappers by
  // their Java-visible concrete classes" -- was asserted nowhere.
  static String lastMessage;

  @SuppressWarnings({ "unchecked", "rawtypes" })
  static Object safelyApply(Processor<?> processor, Object value) {
    try {
      return ((Processor) processor).apply(value);
    }
    catch (ClassCastException ex) {
      lastMessage = ex.getMessage();
      if (startsWithArgumentClassName(ex.getMessage(), value)) {
        return value;
      }
      throw ex;
    }
  }

  // Spring Boot's LambdaSafe.startsWithArgumentClassName, verbatim in behaviour:
  // the bare name, or the "class <name> " prefix HotSpot has used since JDK 11.
  // Testing only the first is a contract no JVM honours -- see this file's header.
  static boolean startsWithArgumentClassName(String message, Object value) {
    if (message == null) {
      return true;
    }
    String name = value.getClass().getName();
    return message.startsWith(name) || message.startsWith("class " + name + " ");
  }

  static void check(String label, Map<?, ?> value) {
    Processor<String> uppercase = candidate -> candidate.toUpperCase();
    Object result = safelyApply(uppercase, value);
    if (result != value) {
      throw new AssertionError(label + " was not returned unchanged");
    }
    System.out.println(label + "=" + value.getClass().getName());
    System.out.println(label + "-msg=" + lastMessage);
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
                "[lambda_safe_unmodifiable_map_classcast] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return;
        }
    }
    assert!(
        status.status.success(),
        "[lambda_safe_unmodifiable_map_classcast] the embedded probe failed to compile — fix the probe source. \
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
        // The MESSAGE, not only `getClass()`. This is the property the file is
        // named for and it was asserted nowhere: `LambdaSafe` reads the message,
        // so a stamp leaking into it (`cratonvm.internal.UnmodifiableMap`) is
        // the failure, whichever of the two accepted wordings carries it.
        for (label, class_name) in [
            ("empty", "java.util.ImmutableCollections$MapN"),
            ("one", "java.util.ImmutableCollections$Map1"),
            ("many", "java.util.ImmutableCollections$MapN"),
            ("unmodifiable", "java.util.Collections$UnmodifiableMap"),
        ] {
            let prefix = format!("{label}-msg=");
            let line = stdout
                .lines()
                .find(|l| l.starts_with(&prefix))
                .unwrap_or_else(|| {
                    panic!(
                        "no `{prefix}` line (nojit={nojit}); stdout:
{stdout}"
                    )
                });
            let message = &line[prefix.len()..];
            let quoted = format!("class {class_name} ");
            assert!(
                message.starts_with(class_name) || message.starts_with(&quoted),
                "the ClassCastException for `{label}` did not name the Java-visible                  class `{class_name}` (nojit={nojit}). Spring Boot's `LambdaSafe`                  accepts exactly these two prefixes, and a private stamp in the                  message is what makes it rethrow.
message: {message}"
            );
        }
        assert!(stdout.contains("OK"), "unexpected output: {stdout}");
    }
}
