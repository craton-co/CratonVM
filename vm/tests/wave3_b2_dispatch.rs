// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-3 Task B² regression: HttpServer.getAddress().getPort() must dispatch
//! onto `java.net.InetSocketAddress.getPort()` (final method that proxies into
//! `holder.getPort()`), not onto a stray `java/lang/String.getPort()`.
//!
//! Pre-fix symptom: the synthetic `alloc_inet_socket_address` helper put a
//! `String` directly at slot 0 of an `InetSocketAddress`, but real-JDK
//! `getPort()` bytecode is `getfield holder; invokevirtual Holder.getPort()`.
//! The sub-`invokevirtual` saw a `String` receiver and tripped a
//! `NoSuchMethodError: java/lang/String.getPort()I`.
//!
//! Fix: `alloc_inet_socket_address` now allocates a real
//! `InetSocketAddress$InetSocketAddressHolder`, populates hostname / addr /
//! port at the correct holder slots (0/1/2), and stores it at the outer
//! object's slot 0 — matching the real-JDK layout exactly. The matching
//! reader (`read_inet_socket_address`) accepts both the legacy synthetic
//! shape and the real-JDK shape.
//!
//! This test pins both:
//!   * the simple `new InetSocketAddress("127.0.0.1", 8080).getPort()` round
//!     trip (covers the `getClass()`/cast-to-Object/cast-back permutations),
//!   * the HttpServer-mediated `srv.getAddress().getPort()` round trip that
//!     was the actual repro from `HttpServer.create(InetSocketAddress(0), 0)`.
//!
//! Subprocess pattern: spawn `cratonvm.exe`, javac the probe sources on
//! demand into a temp dir, assert the four marker lines + the trailing `OK`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const DISPATCH_PROBE_SRC: &str = r#"
import java.net.*;
public class DispatchProbe {
    public static void main(String[] a) throws Exception {
        InetSocketAddress addr = new InetSocketAddress("127.0.0.1", 8080);
        int p = addr.getPort();
        System.out.println("addr.getPort()=" + p);
        System.out.println("addr.class=" + addr.getClass().getName());
        Object o = addr;
        System.out.println("o.class=" + o.getClass().getName());
        InetSocketAddress addr2 = (InetSocketAddress) o;
        int p2 = addr2.getPort();
        System.out.println("addr2.getPort()=" + p2);
        System.out.println("OK");
    }
}
"#;

const HTTP_SERVER_PROBE_SRC: &str = r#"
import com.sun.net.httpserver.HttpServer;
import java.net.*;
public class HttpServerDispatchProbe {
    public static void main(String[] a) throws Exception {
        HttpServer srv = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        InetSocketAddress addr = srv.getAddress();
        System.out.println("addr.class=" + addr.getClass().getName());
        int port = addr.getPort();
        System.out.println("addr.getPort()=" + port);
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

fn java_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("CRATONVM_TEST_JAVA_HOME") {
        let p = PathBuf::from(h);
        if p.join("bin")
            .join(if cfg!(windows) { "javac.exe" } else { "javac" })
            .exists()
        {
            return Some(p);
        }
    }
    // Default real-JDK 25 install location used by the rest of the repo.
    let default = PathBuf::from("C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot");
    if default
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" })
        .exists()
    {
        return Some(default);
    }
    None
}

/// Compile `src` (full source text) as `name.java` into `out_dir`. Returns
/// `false` if javac is unavailable or the compile failed (caller skips).
fn compile_probe(jh: &Path, out_dir: &Path, name: &str, src: &str) -> bool {
    let java_file = out_dir.join(format!("{name}.java"));
    if let Ok(mut f) = std::fs::File::create(&java_file) {
        if f.write_all(src.trim_start().as_bytes()).is_err() {
            return false;
        }
    } else {
        return false;
    }
    let javac = jh
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let compile = Command::new(&javac)
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(out_dir)
        .arg(&java_file)
        .output();
    match compile {
        // javac cannot be launched at all — the one legitimate skip.
        Err(_) => false,
        // javac RAN and rejected the fixture: skipping here would make this
        // test a permanent vacuous pass.
        Ok(o) => {
            // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
            // means this javac is older than the level this probe compiles at, so it never
            // opened the file. That is a missing-toolchain condition — the same one the
            // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
            // source" sends the next reader to edit a correct `.java` file.
            //
            // Narrowly keyed on javac's own wording for an unsupported release, so a
            // genuine source error still reaches the assertion below and still fails loudly
            // (see `probe_compile_guard.rs` for why that must never become a skip).
            if !o.status.success() {
                let stderr_probe = String::from_utf8_lossy(&o.stderr);
                if stderr_probe.contains("release version")
                    && stderr_probe.contains("not supported")
                {
                    eprintln!(
                        "[wave3_b2_dispatch] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[wave3_b2_dispatch] the checked-in probe fixture failed to compile — fix the .java source. \
             javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            out_dir.join(format!("{name}.class")).exists()
        }
    }
}

fn run_probe(bin: &Path, jh: &Path, classes: &Path, name: &str) -> Option<(String, String)> {
    let mut child = Command::new(bin)
        .arg("--java-home")
        .arg(jh)
        .arg("-c")
        .arg(classes)
        .arg(name)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(60) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
    let out = child.wait_with_output().ok()?;
    Some((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn temp_classes_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cratonvm-w3b2-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn w3b2_direct_dispatch_inet_socket_address_get_port() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!("[w3b2] cratonvm binary not found; skipping");
            return;
        }
    };
    let jh = match java_home() {
        Some(j) => j,
        None => {
            eprintln!("[w3b2] javac/java not available; skipping");
            return;
        }
    };
    let classes = temp_classes_dir("direct");
    if !compile_probe(&jh, &classes, "DispatchProbe", DISPATCH_PROBE_SRC) {
        eprintln!("[w3b2] failed to compile DispatchProbe; skipping");
        return;
    }
    let (stdout, stderr) = match run_probe(&bin, &jh, &classes, "DispatchProbe") {
        Some(o) => o,
        None => panic!("[w3b2] DispatchProbe spawn/wait failed"),
    };
    let combined = format!("STDOUT:\n{stdout}\n--- STDERR ---\n{stderr}");
    for needle in [
        "addr.getPort()=8080",
        "addr.class=java.net.InetSocketAddress",
        "o.class=java.net.InetSocketAddress",
        "addr2.getPort()=8080",
    ] {
        assert!(
            combined.contains(needle),
            "DispatchProbe missing line `{needle}`. Output:\n{combined}"
        );
    }
    assert!(
        combined.contains("\nOK") || combined.trim_end().ends_with("OK"),
        "DispatchProbe never printed final OK marker. Output:\n{combined}"
    );
    assert!(
        !combined.contains("java/lang/String.getPort"),
        "DispatchProbe regressed: invokevirtual receiver mis-routed onto String.getPort.\n{combined}"
    );
}

#[test]
fn w3b2_http_server_get_address_get_port() {
    let bin = match cratonvm_binary() {
        Some(b) => b,
        None => {
            eprintln!("[w3b2] cratonvm binary not found; skipping");
            return;
        }
    };
    let jh = match java_home() {
        Some(j) => j,
        None => {
            eprintln!("[w3b2] javac/java not available; skipping");
            return;
        }
    };
    let classes = temp_classes_dir("httpsrv");
    if !compile_probe(
        &jh,
        &classes,
        "HttpServerDispatchProbe",
        HTTP_SERVER_PROBE_SRC,
    ) {
        eprintln!("[w3b2] failed to compile HttpServerDispatchProbe; skipping");
        return;
    }
    let (stdout, stderr) = match run_probe(&bin, &jh, &classes, "HttpServerDispatchProbe") {
        Some(o) => o,
        None => panic!("[w3b2] HttpServerDispatchProbe spawn/wait failed"),
    };
    let combined = format!("STDOUT:\n{stdout}\n--- STDERR ---\n{stderr}");
    assert!(
        combined.contains("addr.class=java.net.InetSocketAddress"),
        "HttpServerDispatchProbe never printed addr.class=java.net.InetSocketAddress.\n{combined}"
    );
    // Port is the OS-assigned ephemeral port — non-zero.
    let port_line = combined
        .lines()
        .find(|l| l.starts_with("addr.getPort()="))
        .unwrap_or("(no addr.getPort() line)");
    assert!(
        port_line.starts_with("addr.getPort()=") && !port_line.ends_with("=0"),
        "HttpServerDispatchProbe addr.getPort() did not return a non-zero ephemeral port.\n\
         observed: {port_line}\n\
         full output:\n{combined}"
    );
    assert!(
        combined.contains("\nOK") || combined.trim_end().ends_with("OK"),
        "HttpServerDispatchProbe never printed final OK marker.\n{combined}"
    );
    assert!(
        !combined.contains("java/lang/String.getPort"),
        "HttpServerDispatchProbe regressed: invokevirtual mis-routed onto String.getPort.\n{combined}"
    );
}
