// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression: every reflective annotation array must carry the component type
//! the JDK declares, not `java.lang.Object`.
//!
//! A reference array stores its COMPONENT class id in `ObjectHeader.class_id`,
//! so the component chosen when the array is built is the class every later
//! reader sees — `arr.getClass().getName()`, a `checkcast`, and (because that
//! is the same header word a virtual/interface dispatch reads to name a
//! receiver's class) any dispatch that reaches the array itself.
//!
//! `build_class_annotation_array` passed `ClassId::new(0)`
//! (`java/lang/Object`) where its Method/Field/Parameter siblings already
//! passed `java/lang/annotation/Annotation`, and all three
//! `getAnnotationsByType` natives passed it too, where the JDK declares `A[]`.
//! So on CratonVM:
//!
//! * `Sub.class.getDeclaredAnnotations().getClass()` was `[Ljava.lang.Object;`
//!   (HotSpot: `[Ljava.lang.annotation.Annotation;`), and
//! * `Base.class.getAnnotationsByType(Tag.class).getClass()` was
//!   `[Ljava.lang.Object;` (HotSpot: `[LTag;`).
//!
//! Found while reducing the `OffsetDateTimeTest` JUnit-discovery crash
//! (`fixed-suite-bugs/hibernate/offsetdatetimetest-junit-discovery-nullptr-sigsegv-20260805-FIXED.md`):
//! JUnit's `AnnotationUtils.findRepeatableAnnotations` walks exactly these
//! arrays, and a dispatch that landed on one reported
//! `NoSuchMethodError: java.lang.Object.annotationType()` — the array's
//! component class, named as if it were the element's.
//!
//! The probe source is embedded below and compiled to a temp dir on the fly,
//! so the test is self-contained. It skips gracefully when javac, a JDK home
//! or the cratonvm binary are unavailable — but a probe that FAILS TO COMPILE
//! is an assertion failure, never a skip.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const PROBE_SRC: &str = r#"
import java.lang.annotation.*;
import java.lang.reflect.*;

public class AnnotationArrayComponentProbe {
    @Retention(RetentionPolicy.RUNTIME)
    @Target({ElementType.TYPE, ElementType.METHOD, ElementType.FIELD,
             ElementType.CONSTRUCTOR, ElementType.PARAMETER})
    public @interface Mark { String v() default "x"; }

    @Retention(RetentionPolicy.RUNTIME) @Target({ElementType.TYPE, ElementType.METHOD})
    @Inherited public @interface Inh { }

    @Retention(RetentionPolicy.RUNTIME) @Target({ElementType.TYPE, ElementType.METHOD})
    public @interface Tags { Tag[] value(); }

    @Retention(RetentionPolicy.RUNTIME) @Target({ElementType.TYPE, ElementType.METHOD})
    @Repeatable(Tags.class) public @interface Tag { String value(); }

    @Mark @Inh @Tag("a") @Tag("b") public static class Base { }

    @Mark public static class Sub extends Base {
        @Mark public int f;
        @Mark public Sub(@Mark int p) { }
        @Mark @Tag("m1") @Tag("m2") public void m(@Mark int p) { }
    }

    static void comp(String key, Object arr) {
        System.out.println(key + "=" + arr.getClass().getName());
    }

    public static void main(String[] args) throws Exception {
        Method m = Sub.class.getDeclaredMethod("m", int.class);
        Field f = Sub.class.getDeclaredField("f");
        Constructor<?> c = Sub.class.getDeclaredConstructor(int.class);

        comp("classDeclared", Sub.class.getDeclaredAnnotations());
        comp("classAll", Sub.class.getAnnotations());
        comp("methodDeclared", m.getDeclaredAnnotations());
        comp("methodAll", m.getAnnotations());
        comp("fieldDeclared", f.getDeclaredAnnotations());
        comp("ctorDeclared", c.getDeclaredAnnotations());
        comp("paramDeclared", m.getParameters()[0].getDeclaredAnnotations());
        comp("classByType", Base.class.getAnnotationsByType(Tag.class));
        comp("classDeclaredByType", Base.class.getDeclaredAnnotationsByType(Tag.class));
        comp("methodByType", m.getAnnotationsByType(Tag.class));

        // The header word the arrays carry is what a `checkcast` and an
        // `instanceof` read, so assert the language-level view too.
        System.out.println("classDeclaredIsAnnotationArray="
            + (Sub.class.getDeclaredAnnotations() instanceof Annotation[]
               && Sub.class.getDeclaredAnnotations().getClass() == Annotation[].class));
        System.out.println("classByTypeIsTagArray="
            + (Base.class.getAnnotationsByType(Tag.class).getClass() == Tag[].class));

        // Every element must still answer annotationType() — the JUnit
        // `findRepeatableAnnotations` step the crash was found on.
        StringBuilder types = new StringBuilder();
        for (AnnotatedElement e : new AnnotatedElement[] { Sub.class, Base.class, m, f, c }) {
            Annotation[] anns = e.getDeclaredAnnotations();
            for (Annotation a : anns) {
                types.append(a.annotationType().getSimpleName()).append(',');
            }
        }
        System.out.println("annotationTypes=" + types);
        System.out.println("OK");
    }
}
"#;

mod common;

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
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target");
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

fn jdk_home() -> Option<PathBuf> {
    common::require_jdk(jdk_home_lookup())
}

/// The JDK moves around on developer boxes (`jdk-25`, `jdk-25.0.2.10-hotspot`,
/// `jdk-25.0.3.9-hotspot`, …), so probe the install roots rather than pinning a
/// point release that ages out and turns this test into a silent skip.
fn jdk_home_lookup() -> Option<PathBuf> {
    for var in &["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(j) = std::env::var(var) {
            let p = PathBuf::from(&j);
            if p.join("bin").exists() {
                return Some(p);
            }
        }
    }
    for root in [
        "C:/Program Files/Eclipse Adoptium",
        "C:/Program Files/Java",
        "/usr/lib/jvm",
    ] {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("jdk-2") || n.contains("java-2"))
                    && p.join("bin").exists()
            })
            .collect();
        candidates.sort();
        if let Some(best) = candidates.pop() {
            return Some(best);
        }
    }
    None
}

fn compile_probe(javac: &Path) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cratonvm-annotation-array-component-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("AnnotationArrayComponentProbe.java");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    for stale in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let p = stale.path();
        if p.extension().and_then(|e| e.to_str()) == Some("class") {
            let _ = std::fs::remove_file(p);
        }
    }
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!("[annotation_array_component] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    // A javac too old for `--release 21` never opened the file: a missing
    // toolchain, not a broken probe. Narrowly keyed on javac's own wording so a
    // genuine source error still reaches the assertion below and fails loudly.
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[annotation_array_component] javac cannot target --release 21 ({}); skipping.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("AnnotationArrayComponentProbe.class").exists(),
        "[annotation_array_component] the embedded probe failed to compile — fix the probe \
         source. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(bin: &Path, jdk: &Path, classes: &Path, nojit: bool) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("--java-home").arg(jdk);
    if nojit {
        cmd.arg("--nojit");
    }
    cmd.arg("-cp")
        .arg(classes)
        .arg("AnnotationArrayComponentProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn cratonvm");
    let timeout = Duration::from_secs(180);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[annotation_array_component] probe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[annotation_array_component] try_wait failed: {e}"),
        }
    }
    let out = child.wait_with_output().expect("collect output");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// The component type the JDK declares for each accessor. Every entry is the
/// value HotSpot prints for the same probe.
const EXPECTED: &[(&str, &str)] = &[
    ("classDeclared", "[Ljava.lang.annotation.Annotation;"),
    ("classAll", "[Ljava.lang.annotation.Annotation;"),
    ("methodDeclared", "[Ljava.lang.annotation.Annotation;"),
    ("methodAll", "[Ljava.lang.annotation.Annotation;"),
    ("fieldDeclared", "[Ljava.lang.annotation.Annotation;"),
    ("ctorDeclared", "[Ljava.lang.annotation.Annotation;"),
    ("paramDeclared", "[Ljava.lang.annotation.Annotation;"),
    ("classByType", "[LAnnotationArrayComponentProbe$Tag;"),
    (
        "classDeclaredByType",
        "[LAnnotationArrayComponentProbe$Tag;",
    ),
    ("methodByType", "[LAnnotationArrayComponentProbe$Tag;"),
];

fn assert_components(stdout: &str, stderr: &str, mode: &str) {
    let line = |key: &str| -> String {
        stdout
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| {
                panic!(
                    "[annotation_array_component/{mode}] missing `{key}` line.\n\
                     stdout:\n{stdout}\nstderr:\n{stderr}"
                )
            })
            .to_string()
    };
    for (key, want) in EXPECTED {
        assert_eq!(
            &line(key),
            want,
            "[{mode}] {key}: reflective annotation array must carry the JDK's component type"
        );
    }
    assert_eq!(
        line("classDeclaredIsAnnotationArray"),
        "true",
        "[{mode}] Class.getDeclaredAnnotations() must be an Annotation[] to \
         `instanceof` and to `getClass() ==`"
    );
    assert_eq!(
        line("classByTypeIsTagArray"),
        "true",
        "[{mode}] getAnnotationsByType(Tag.class) must be a Tag[]"
    );
    // The elements themselves must survive the change — an array retyped but
    // populated with the wrong thing would pass every check above.
    let types = line("annotationTypes");
    for want in ["Mark", "Inh", "Tags"] {
        assert!(
            types.contains(want),
            "[{mode}] expected `{want}` among the reflected annotation types, got `{types}`"
        );
    }
    assert!(
        stdout.contains("OK"),
        "[{mode}] probe did not finish.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn reflective_annotation_arrays_carry_the_declared_component_type() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[annotation_array_component] cratonvm binary not found; \
                 build with `cargo build --release -p cratonvm-cli`. Skipping."
            );
            return;
        }
    };
    let jdk = match jdk_home() {
        Some(j) => j,
        None => {
            eprintln!(
                "[annotation_array_component] no JDK home \
                 (set CRATONVM_TEST_JDK or JAVA_HOME); skipping"
            );
            return;
        }
    };
    let javac = jdk
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let classes = match compile_probe(&javac) {
        Some(d) => d,
        None => return,
    };

    for (mode, nojit) in [("nojit", true), ("jit", false)] {
        let (stdout, stderr) = run_probe(&bin, &jdk, &classes, nojit);
        assert_components(&stdout, &stderr, mode);
    }
}
