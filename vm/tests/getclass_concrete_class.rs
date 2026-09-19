// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression: `Object.getClass()` must report a concrete, JDK-plausible class
//! for synthetic objects.
//!
//! Several CratonVM synthetic factories stamp their result with the *interface*
//! or *abstract* type it stands in for (e.g. `IntStream.rangeClosed(..)` → the
//! `java/util/stream/IntStream` interface, `FileSystems.getDefault()` → the
//! abstract `java/nio/file/FileSystem`) or with a private craton-internal name
//! (`List.of(..)` → `cratonvm/internal/UnmodifiableList`). On a real JVM an
//! instance's runtime class is always concrete, so `getClass()` must never
//! surface an interface/abstract/internal type.
//!
//! The fix lives in `native_object_get_class` (native-builtins/src/lib.rs): a
//! memoised stamp→concrete-class substitution that leaves storage, dispatch,
//! and GC layout untouched (it changes only the Java-visible `Class` mirror).
//!
//! The probe source is embedded below and compiled to a temp dir on the fly, so
//! the test is self-contained (no gitignored `apps/` fixture required). It
//! skips gracefully when `javac`, a JDK home, or the cratonvm binary are
//! unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const PROBE_SRC: &str = r#"
import java.net.URI;
import java.net.URLConnection;
import java.nio.file.FileSystems;
import java.util.*;
import java.util.stream.IntStream;
import java.util.stream.Stream;

public class GetClassProbe {
  static String c(Object o){return o.getClass().getName();}
  public static void main(String[] a) throws Exception {
    System.out.println("intRange=" + c(IntStream.rangeClosed(1, 4)));
    System.out.println("intRangeVals=" + Arrays.toString(IntStream.rangeClosed(1, 4).toArray()));
    System.out.println("longVals=" + Arrays.toString(java.util.stream.LongStream.of(11L, 13L).toArray()));
    System.out.println("doubleVals=" + Arrays.toString(java.util.stream.DoubleStream.of(2.5, 4.5).toArray()));
    System.out.println("mapToIntVals=" + Arrays.toString(Stream.of("bbb", "a").mapToInt(String::length).toArray()));
    // Immutable factories -> ImmutableCollections (size-discriminated).
    System.out.println("listEmpty=" + c(List.of()));
    System.out.println("list2=" + c(List.of(1, 2)));
    System.out.println("list3=" + c(List.of(1, 2, 3)));
    System.out.println("set2=" + c(Set.of(1, 2)));
    System.out.println("map1=" + c(Map.of(1, 2)));
    System.out.println("copyOf=" + c(List.copyOf(new ArrayList<>(List.of(1, 2)))));
    // Unmodifiable wrappers -> Collections$Unmodifiable* (lists split on RandomAccess).
    System.out.println("unmodRA=" + c(Collections.unmodifiableList(new ArrayList<>(List.of(1, 2)))));
    System.out.println("unmodLL=" + c(Collections.unmodifiableList(new LinkedList<>(List.of(1, 2)))));
    System.out.println("unmodSet=" + c(Collections.unmodifiableSet(new HashSet<>(Set.of(1)))));
    System.out.println("unmodMap=" + c(Collections.unmodifiableMap(new HashMap<>(Map.of(1, 2)))));
    System.out.println("unmodColl=" + c(Collections.unmodifiableCollection(new ArrayList<>(List.of(1)))));
    // Real-JDK singleton family — class + behavior (size must be 1 via the
    // interface-native delegation to real bytecode).
    java.util.List<Integer> sl = Collections.singletonList(7);
    System.out.println("singletonList=" + c(sl) + "|sz=" + sl.size() + "|get=" + sl.get(0));
    System.out.println("singletonSet=" + c(Collections.singleton(5)) + "|sz=" + Collections.singleton(5).size());
    java.util.Map<Integer,Integer> sm = Collections.singletonMap(3, 30);
    System.out.println("singletonMap=" + c(sm) + "|sz=" + sm.size() + "|get=" + sm.get(3));
    System.out.println("fs=" + c(FileSystems.getDefault()));
    URLConnection jc = URI.create("jar:file:/none.jar!/x").toURL().openConnection();
    System.out.println("jarConn=" + c(jc));
    System.out.println("OK");
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

/// Prerequisite gate: the lookup below is unchanged — only a MISSING JDK is
/// reported differently. See `common::require_jdk`.
fn jdk_home() -> Option<PathBuf> {
    common::require_jdk(jdk_home_lookup())
}

fn jdk_home_lookup() -> Option<PathBuf> {
    for var in &["CRATONVM_TEST_JDK", "JAVA_HOME"] {
        if let Ok(j) = std::env::var(var) {
            let p = PathBuf::from(&j);
            if p.exists() {
                return Some(p);
            }
        }
    }
    for cand in [
        "C:/Program Files/Java/jdk-25",
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Write + compile the embedded probe to a fresh temp dir. Returns the
/// directory holding `GetClassProbe.class`, or `None` if javac is unavailable.
fn compile_probe(javac: &Path) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cratonvm-getclass-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("GetClassProbe.java");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(dir.join("GetClassProbe.class"));
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
            eprintln!("[getclass_concrete_class] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
    // means this javac is older than the level this probe compiles at, so it never
    // opened the file. That is a missing-toolchain condition — the same one the
    // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
    // source" sends the next reader to edit a correct `.java` file.
    //
    // Narrowly keyed on javac's own wording for an unsupported release, so a
    // genuine source error still reaches the assertion below and still fails loudly
    // (see `probe_compile_guard.rs` for why that must never become a skip).
    if !out.status.success() {
        let stderr_probe = String::from_utf8_lossy(&out.stderr);
        if stderr_probe.contains("release version") && stderr_probe.contains("not supported") {
            eprintln!(
                "[getclass_concrete_class] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && dir.join("GetClassProbe.class").exists(),
        "[getclass_concrete_class] the embedded probe failed to compile — fix the probe source. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

#[test]
fn getclass_reports_concrete_classes() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!(
                "[getclass_concrete_class] cratonvm binary not found; \
                 build with `cargo build --release -p cratonvm-cli`. Skipping."
            );
            return;
        }
    };
    let jdk = match jdk_home() {
        Some(j) => j,
        None => {
            eprintln!(
                "[getclass_concrete_class] no JDK home \
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
        None => {
            eprintln!("[getclass_concrete_class] javac unavailable; skipping");
            return;
        }
    };

    let mut child = match Command::new(&bin)
        .arg("--java-home")
        .arg(&jdk)
        .arg("-c")
        .arg(&classes)
        .arg("GetClassProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[getclass_concrete_class] failed to spawn cratonvm: {e}");
            return;
        }
    };
    let timeout = Duration::from_secs(120);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[getclass_concrete_class] GetClassProbe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[getclass_concrete_class] try_wait failed: {e}"),
        }
    }
    let output = child.wait_with_output().expect("collect output");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let line = |key: &str| -> String {
        stdout
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_else(|| {
                panic!("[getclass_concrete_class] missing `{key}` line.\nstdout:\n{stdout}\nstderr:\n{stderr}")
            })
            .to_string()
    };

    // Platform-independent exact matches (== HotSpot jdk-25).
    assert_eq!(
        line("intRange="),
        "java.util.stream.IntPipeline$Head",
        "IntStream.rangeClosed must report a concrete IntPipeline class, not the interface"
    );
    assert_eq!(line("intRangeVals="), "[1, 2, 3, 4]");
    assert_eq!(line("longVals="), "[11, 13]");
    assert_eq!(line("doubleVals="), "[2.5, 4.5]");
    assert_eq!(line("mapToIntVals="), "[3, 1]");
    // Immutable factories.
    assert_eq!(line("listEmpty="), "java.util.ImmutableCollections$ListN");
    assert_eq!(line("list2="), "java.util.ImmutableCollections$List12");
    assert_eq!(line("list3="), "java.util.ImmutableCollections$ListN");
    assert_eq!(line("set2="), "java.util.ImmutableCollections$Set12");
    assert_eq!(line("map1="), "java.util.ImmutableCollections$Map1");
    assert_eq!(line("copyOf="), "java.util.ImmutableCollections$List12");
    // Unmodifiable wrappers — distinct from the immutable family, and lists
    // split on RandomAccess (ArrayList vs LinkedList backing).
    assert_eq!(
        line("unmodRA="),
        "java.util.Collections$UnmodifiableRandomAccessList"
    );
    assert_eq!(line("unmodLL="), "java.util.Collections$UnmodifiableList");
    assert_eq!(line("unmodSet="), "java.util.Collections$UnmodifiableSet");
    assert_eq!(line("unmodMap="), "java.util.Collections$UnmodifiableMap");
    assert_eq!(
        line("unmodColl="),
        "java.util.Collections$UnmodifiableCollection"
    );
    // Real-JDK singletons: concrete class AND correct behavior (size==1, the
    // proof that the interface-native delegation to real bytecode works).
    assert_eq!(
        line("singletonList="),
        "java.util.Collections$SingletonList|sz=1|get=7"
    );
    assert_eq!(
        line("singletonSet="),
        "java.util.Collections$SingletonSet|sz=1"
    );
    assert_eq!(
        line("singletonMap="),
        "java.util.Collections$SingletonMap|sz=1|get=30"
    );
    assert_eq!(
        line("jarConn="),
        "sun.net.www.protocol.jar.JarURLConnection",
        "jar: URLConnection must report the concrete impl, not the abstract class"
    );

    // FileSystem concrete class is platform specific (WindowsFileSystem /
    // LinuxFileSystem / …); just require a concrete `sun.nio.fs.*`, never the
    // abstract `java.nio.file.FileSystem`.
    let fs = line("fs=");
    assert!(
        fs.starts_with("sun.nio.fs.") && fs != "java.nio.file.FileSystem",
        "FileSystems.getDefault() must report a concrete sun.nio.fs.* class, got `{fs}`"
    );

    assert!(
        stdout.contains("OK"),
        "GetClassProbe did not reach OK marker.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        output.status.success(),
        "GetClassProbe exited non-zero: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
}
