// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression: a sealed class defined by the **bootstrap** loader must resolve
//! ALL of its permitted subclasses, and `getPermittedSubclasses0()` must never
//! hand real JDK bytecode an array containing a null element.
//!
//! `native_class_get_permitted_subclasses` resolves each name from the
//! classfile's `PermittedSubclasses` attribute through
//! `resolve_nestmate_via_defining_loader`. That helper drives the sealed
//! class's own `ClassLoader.loadClass` — an ACTIVE resolution — but only when
//! there IS a Java-level loader object. A JDK-owned class has none
//! (`Class.getClassLoader()` is null for every `java/`, `javax/`, `jdk/`,
//! `sun/`, `com/sun/` name), so it fell through to `ctx.class_id_by_name`, a
//! PASSIVE cache probe that never triggers classloading, and reported only
//! whichever permitted subclasses some earlier code happened to have loaded.
//!
//! Measured before the fix on JDK 25's `java.security.DEREncodable` (sealed,
//! 8 permitted subclasses, the PEM-encoding JEP):
//!
//! ```text
//! getPermittedSubclasses0() ->
//!   [null, null, null, null, null, X509Certificate, null, null]
//! ```
//!
//! Real HotSpot 25.0.3 returns all 8. Those nulls then reach
//! `Class.getPermittedSubclasses()`'s `isDirectSubType(c)` filter, which calls
//! `c.getInterfaces(false)` with no null guard — aborting every Mockito
//! `mock(X509Certificate.class)` with
//! `NullPointerException: Cannot read field "interfaces" because "rd" is null`
//! (`fixed-suite-bugs/springboot/`
//! `sealed-derencodable-getinterfaces-npe-mockito-x509-FIXED.md`).
//!
//! The probe reads `getPermittedSubclasses0()` reflectively — the raw native,
//! before the public wrapper's filter — so a regression shows up as null slots
//! rather than being silently laundered into an empty array.
//!
//! Skips when no cratonvm binary, no JDK home, no `javac`, or a JDK whose
//! `java.security.DEREncodable` does not exist (pre-25).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const PROBE_SRC: &str = r#"
import java.lang.reflect.Method;

public class SealedBootstrapProbe {
  public static void main(String[] a) throws Exception {
    Class<?> der;
    try {
      der = Class.forName("java.security.DEREncodable");
    } catch (Throwable t) {
      System.out.println("NO-DERENCODABLE");
      return;
    }
    // Raw native first: nothing in this process has touched any permitted
    // subclass yet, which is exactly the ordering that used to fail.
    Method m = Class.class.getDeclaredMethod("getPermittedSubclasses0");
    m.setAccessible(true);
    Class<?>[] raw = (Class<?>[]) m.invoke(der);
    StringBuilder sb = new StringBuilder();
    int nulls = 0;
    if (raw == null) {
      sb.append("null");
    } else {
      for (int i = 0; i < raw.length; i++) {
        if (i > 0) sb.append(',');
        if (raw[i] == null) { nulls++; sb.append("NULL"); }
        else sb.append(raw[i].getName());
      }
    }
    System.out.println("RAW0 n=" + (raw == null ? -1 : raw.length)
        + " nulls=" + nulls + " [" + sb + "]");
    System.out.println("SEALED " + der.isSealed());
    Class<?>[] pub = der.getPermittedSubclasses();
    System.out.println("PUBLIC n=" + (pub == null ? -1 : pub.length));
    // getNestMembers0() shares the same resolver, and had the same hole:
    // nothing in this process has touched a nest member of these hosts.
    for (String host : new String[] {
        "java.lang.Character",
        "java.lang.ProcessBuilder",
        "java.util.concurrent.ConcurrentHashMap",
    }) {
      Class<?>[] members = Class.forName(host).getNestMembers();
      int memberNulls = 0;
      for (Class<?> x : members) if (x == null) memberNulls++;
      System.out.println("NEST " + host + " n=" + members.length + " nulls=" + memberNulls);
    }
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
        "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot",
        "C:/Program Files/Java/jdk-25",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// `None` means "no usable `javac` on this machine" — a legitimate skip.
///
/// A javac that RUNS and rejects the source is NOT a skip: it means this file's
/// embedded probe no longer compiles, and silently returning `None` there turns
/// the whole test into a vacuous pass. That is not hypothetical — the first
/// draft of the nest-member assertions below shadowed two locals, javac
/// errored, and the test reported `ok` in 0.8 s against a binary that
/// reproduces the bug perfectly. Panic instead.
fn compile_probe(javac: &Path) -> Option<PathBuf> {
    if !javac.exists() {
        return None;
    }
    let dir = std::env::temp_dir().join("cratonvm-sealed-bootstrap-probe");
    let _ = std::fs::create_dir_all(&dir);
    let src = dir.join("SealedBootstrapProbe.java");
    let class_file = dir.join("SealedBootstrapProbe.class");
    // Never let a stale .class from an earlier revision stand in for a source
    // that no longer compiles.
    let _ = std::fs::remove_file(&class_file);
    std::fs::write(&src, PROBE_SRC).expect("write probe source");
    let out = match Command::new(javac)
        .args(["--release", "21", "-d"])
        .arg(&dir)
        .arg(&src)
        .output()
    {
        Ok(o) => o,
        // javac present but unrunnable (permissions, broken install) — skip.
        Err(e) => {
            eprintln!("[sealed_bootstrap] javac could not be executed: {e}; skipping");
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
                "[sealed_bootstrap_permitted_subclasses] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    assert!(
        out.status.success() && class_file.exists(),
        "[sealed_bootstrap] the embedded probe failed to compile — fix PROBE_SRC. \
         javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

#[test]
fn bootstrap_sealed_class_resolves_every_permitted_subclass() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!(
            "[sealed_bootstrap] cratonvm binary not found; build with \
             `cargo build --release -p cratonvm-cli`. Skipping."
        );
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!("[sealed_bootstrap] no JDK home (set CRATONVM_TEST_JDK or JAVA_HOME); skipping");
        return;
    };
    let javac = jdk
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let Some(classes) = compile_probe(&javac) else {
        eprintln!("[sealed_bootstrap] javac unavailable; skipping");
        return;
    };

    let mut child = match Command::new(&bin)
        .arg("--java-home")
        .arg(&jdk)
        // The probe reflects on `Class.getPermittedSubclasses0`, a PRIVATE
        // NATIVE of `java.lang.Class`, and calls `setAccessible(true)` on it.
        // Since JDK 17's strong encapsulation that needs an explicit open, and
        // without it the VM answers
        // `InaccessibleObjectException: module java.base does not "opens
        // java.lang" to unnamed module` — which is CORRECT, and is what this
        // test was reading as a VM defect.
        //
        // Measured on the same JDK 25 this test runs against, rather than
        // assumed: `java -cp . Ctl` throws the same exception with the same
        // wording, and `java --add-opens java.base/java.lang=ALL-UNNAMED`
        // succeeds. CratonVM was matching HotSpot; the harness was asking for
        // something no JVM grants by default.
        .arg("--add-opens")
        .arg("java.base/java.lang=ALL-UNNAMED")
        .arg("--Xmx")
        .arg("1g")
        .arg("-cp")
        .arg(&classes)
        .arg("SealedBootstrapProbe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[sealed_bootstrap] failed to spawn cratonvm: {e}");
            return;
        }
    };
    let timeout = Duration::from_secs(180);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[sealed_bootstrap] probe timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("[sealed_bootstrap] try_wait failed: {e}"),
        }
    }
    let out = child.wait_with_output().expect("probe output");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if stdout.contains("NO-DERENCODABLE") {
        eprintln!("[sealed_bootstrap] JDK has no java.security.DEREncodable (pre-25); skipping");
        return;
    }
    assert!(
        stdout.contains("OK"),
        "[sealed_bootstrap] probe did not complete.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let raw_line = stdout
        .lines()
        .find(|l| l.starts_with("RAW0 "))
        .unwrap_or_else(|| panic!("[sealed_bootstrap] no RAW0 line.\nstdout:\n{stdout}"));

    // The exact defect: holes in the array handed to real JDK bytecode.
    assert!(
        raw_line.contains("nulls=0"),
        "[sealed_bootstrap] getPermittedSubclasses0() returned null element(s) for the \
         bootstrap-loaded sealed interface java.security.DEREncodable — the passive \
         `class_id_by_name` fallback is back. Line: {raw_line}"
    );
    // JDK 25 seals DEREncodable over exactly 8 permitted subclasses. Assert on
    // the count, not the identities, so a JDK 26 that adds one still passes.
    assert!(
        raw_line.contains("n=8"),
        "[sealed_bootstrap] expected 8 permitted subclasses for java.security.DEREncodable \
         (real HotSpot 25.0.3: AsymmetricKey, KeyPair, PKCS8EncodedKeySpec, \
          X509EncodedKeySpec, EncryptedPrivateKeyInfo, X509Certificate, X509CRL, PEMRecord). \
         Line: {raw_line}"
    );
    assert!(
        stdout.contains("SEALED true"),
        "[sealed_bootstrap] DEREncodable.isSealed() must be true.\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("PUBLIC n=8"),
        "[sealed_bootstrap] the public getPermittedSubclasses() wrapper must survive its own \
         isDirectSubType filter with all 8 entries (a null slot silently collapses it to \
         `[]`).\nstdout:\n{stdout}"
    );

    // `getNestMembers0()` shares `resolve_nestmate_via_defining_loader`, so it
    // carried the identical passive-lookup hole — and reported it more loudly.
    // Measured pre-fix / post-fix / real HotSpot 25.0.3:
    //   java.lang.Character                       1 / 5  / 5
    //   java.lang.ProcessBuilder                  1 / 12 / 12
    //   java.util.concurrent.ConcurrentHashMap   16 / 54 / 54
    // Assert a floor rather than the exact count so a JDK that adds a nested
    // class still passes; the floors are far above the pre-fix values.
    for (host, floor) in [
        ("java.lang.Character", 5usize),
        ("java.lang.ProcessBuilder", 12),
        ("java.util.concurrent.ConcurrentHashMap", 54),
    ] {
        let line = stdout
            .lines()
            .find(|l| l.starts_with(&format!("NEST {host} ")))
            .unwrap_or_else(|| {
                panic!("[sealed_bootstrap] no NEST line for {host}.\nstdout:\n{stdout}")
            });
        let n: usize = line
            .split(" n=")
            .nth(1)
            .and_then(|s| s.split(' ').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("[sealed_bootstrap] unparseable NEST line: {line}"));
        assert!(
            line.contains("nulls=0"),
            "[sealed_bootstrap] getNestMembers() returned a null element for {host}: {line}"
        );
        assert!(
            n >= floor,
            "[sealed_bootstrap] {host} reported only {n} nest members (real HotSpot 25.0.3: \
             {floor}). The bootstrap-loaded nest host is back on the passive \
             `class_id_by_name` lookup and can only see members something else already \
             loaded. Line: {line}"
        );
    }
}
