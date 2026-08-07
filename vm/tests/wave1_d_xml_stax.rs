// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 1, Task D — `apps/xml_probe/XmlProbe.java` regression test.
//!
//! Pin the JAXP / StAX (`javax.xml.stream`) cursor API end-to-end against
//! the cratonvm CLI in real-JDK mode. Spawns a subprocess, feeds it a
//! tiny XML fixture, and asserts the probe parses it correctly.
//!
//! Required output lines (`STEP 5` of the Wave 1.D method):
//!   * `servers=2`     — both `<server/>` elements were observed.
//!   * `firstName=alpha` — `getAttributeValue(null, "name")` returned
//!     the first server's `name` attribute.
//!   * `OK`            — the probe reached its final line, proving the
//!     full StAX cursor walk (`hasNext`/`next`/`getLocalName`/
//!     `getAttributeValue`/`getText`/`close`) ran end-to-end without
//!     throwing.
//!
//! The `events>=10` and `cdata.contains.bracketed=true` lines are
//! "nice-to-have" per the wave method and are not asserted here — they
//! can vary between the synthetic StAX path (driven by `quick-xml`) and
//! the JDK Xerces path (Apache `XMLStreamReader` impl) depending on
//! which path our native registration intercepts at this build.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn worktree_root() -> PathBuf {
    manifest_dir().parent().unwrap().to_path_buf()
}

fn probe_dir() -> PathBuf {
    worktree_root().join("apps").join("xml_probe")
}

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
    let target = worktree_root().join("target");
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

fn java_home() -> Option<String> {
    if let Ok(h) = std::env::var("CRATONVM_JAVA_HOME") {
        return Some(h);
    }
    if let Ok(h) = std::env::var("JAVA_HOME") {
        return Some(h);
    }
    let candidate = "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot";
    if Path::new(candidate).exists() {
        return Some(candidate.to_string());
    }
    None
}

/// Write the canonical wave-1.D fixture. Mirrors `STEP 2` of the method:
/// `<config>` with two `<server/>` children and a CDATA `<metadata/>`
/// payload that contains a literal `<bracketed>` token.
fn write_fixture() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cratonvm-wave1-d");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[wave1-d] failed to create fixture dir {:?}: {e}", dir);
        return None;
    }
    let path = dir.join("test.xml");
    let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                <config>\n  \
                  <server name=\"alpha\" port=\"8080\"/>\n  \
                  <server name=\"beta\" port=\"8081\"/>\n  \
                  <metadata><![CDATA[some <bracketed> & escaped data]]></metadata>\n\
                </config>\n";
    if let Err(e) = std::fs::write(&path, body) {
        eprintln!("[wave1-d] failed to write fixture {:?}: {e}", path);
        return None;
    }
    // The probe in `apps/xml_probe/XmlProbe.java` reads `/tmp/test.xml`.
    // On Windows there is no `/tmp` by default, so duplicate the file
    // to that exact path when possible — mkdir+write is a no-op when it
    // already exists. If the platform refuses (read-only root), the
    // probe-side `FileInputStream` will fail and we'll skip the test.
    let tmp = PathBuf::from("/tmp");
    let _ = std::fs::create_dir_all(&tmp);
    let _ = std::fs::write(tmp.join("test.xml"), body);
    Some(path)
}

fn run_xml_probe(timeout: Duration) -> Option<(String, String, Option<i32>)> {
    let bin = cratonvm_binary()?;
    let probe = probe_dir();
    if !probe.join("XmlProbe.class").exists() {
        // Loud, and a failure under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        let _ = common::require_fixture(
            "wave1-d",
            "the Wave 1 Task D fixture `XmlProbe` (XmlProbe.class, compiled from XmlProbe.java)",
            &[probe.join("XmlProbe.class"), probe.join("XmlProbe.java")],
        );
        return None;
    }
    write_fixture()?;
    let mut cmd = Command::new(&bin);
    if let Some(home) = java_home() {
        cmd.arg("--java-home").arg(&home);
    }
    cmd.arg("-c").arg(&probe).arg("XmlProbe");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[wave1-d] failed to spawn cratonvm: {e}");
            return None;
        }
    };
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("[wave1-d] XmlProbe timed out after {:?}", timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[wave1-d] try_wait failed: {e}");
                return None;
            }
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[wave1-d] wait_with_output failed: {e}");
            return None;
        }
    };
    Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    ))
}

#[test]
fn xml_probe_parses_servers_and_first_name() {
    let (stdout, stderr, rc) = match run_xml_probe(Duration::from_secs(60)) {
        Some(o) => o,
        None => {
            eprintln!("[wave1-d] skipping (binary or probe class unavailable)");
            return;
        }
    };
    assert_eq!(
        rc,
        Some(0),
        "wave1-d: cratonvm exited rc={:?}, stdout={:?}, stderr={:?}",
        rc,
        stdout,
        stderr
    );
    assert!(
        stdout.contains("servers=2"),
        "wave1-d: XmlProbe must report `servers=2`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("firstName=alpha"),
        "wave1-d: XmlProbe must report `firstName=alpha`. Got stdout={:?}",
        stdout
    );
    assert!(
        stdout.contains("OK"),
        "wave1-d: XmlProbe must reach the final `OK` line. Got stdout={:?}",
        stdout
    );
}
