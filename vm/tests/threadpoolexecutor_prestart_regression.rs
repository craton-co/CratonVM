//! Regression coverage for the Tomcat endpoint-worker prestart flake.
//!
//! A real JDK `ThreadPoolExecutor` prestarts a newly constructed worker in
//! exactly the same way as Tomcat's endpoint executor. The probe must never
//! observe `IllegalThreadStateException` for that fresh worker.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn probe_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("resources")
        .join("cratonvm")
        .join("ThreadPoolExecutorPrestartProbe.java")
}

mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let path = PathBuf::from(bin);
        if path.exists() {
            return Some(path);
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?;
    let executable = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    ["release", "debug"]
        .iter()
        .map(|profile| root.join("target").join(profile).join(executable))
        .find(|path| path.exists())
}

fn compile_probe() -> Option<PathBuf> {
    let output_dir = std::env::temp_dir().join(format!(
        "cratonvm-threadpoolexecutor-prestart-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&output_dir);
    fs::create_dir_all(&output_dir).ok()?;
    let out = match Command::new("javac")
        .args(["--release", "21", "-d"])
        .arg(&output_dir)
        .arg(probe_source())
        .output()
    {
        Ok(o) => o,
        // javac cannot be launched at all — the one legitimate skip.
        Err(e) => {
            eprintln!("[threadpoolexecutor_prestart] javac could not be executed: {e}; skipping");
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
                "[threadpoolexecutor_prestart_regression] javac cannot target --release 21 ({}); skipping. Point \
                 JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                stderr_probe.lines().next().unwrap_or("").trim()
            );
            return None;
        }
    }
    // javac RAN and rejected the fixture: skipping here would make this test a
    // permanent vacuous pass.
    assert!(
        out.status.success(),
        "[threadpoolexecutor_prestart] the checked-in probe fixture failed to compile \
         — fix the .java source. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(output_dir)
}

#[test]
fn real_jdk_thread_pool_prestart_keeps_fresh_threads_startable() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[threadpoolexecutor_prestart] cratonvm binary unavailable; skipping");
        return;
    };
    // `compile_probe` returns `None` for the two toolchain conditions it skips
    // on — javac absent, and a javac too old for the `--release` this probe
    // compiles at. `.expect()` turned both into a panic that read like a
    // compile failure.
    let Some(probe_classes) = compile_probe() else {
        eprintln!("[threadpoolexecutor_prestart] probe could not be compiled; skipping");
        return;
    };

    let mut child = Command::new(binary)
        .args([
            "-c",
            probe_classes.to_str().unwrap(),
            "cratonvm.ThreadPoolExecutorPrestartProbe",
            "2000",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ThreadPoolExecutor prestart probe");
    // Was: a 120 s deadline polled with `try_wait` over piped stdio that
    // nothing read until after the child exited. Both halves of that were
    // wrong.
    //
    // The deadline, because 2000 iterations of this probe take **147 s real
    // against 28 s user** with a debug binary on a busy 8-core host, and print
    // `PRESTART_OK iterations=2000 workers=2000` when they get there — every
    // assertion satisfied, the clock the only thing that failed, and the
    // real/user ratio saying the host was busy rather than the VM slow. A
    // deadline cannot tell "slow" from "stuck"; `remaining=` can, so the probe
    // prints it and this watches it. See `common::Progress`.
    //
    // The poll loop, because it never read either pipe. A Linux pipe holds
    // 64 KiB; a probe that writes more blocks in `write` and can never exit,
    // and the parent reports a hang for a child that is waiting on the parent.
    // This probe writes little today — but it now writes 40 progress lines it
    // did not write before, and the VM behind it emitted 4.2 MB of stderr on
    // one measured `VthreadProbe` run. `common::wait_watching` drains both
    // pipes on their own threads for exactly that reason.
    let watched = common::wait_watching(
        child,
        common::Progress {
            countdown_key: "remaining=",
            // A starvation budget, not a runtime one: the probe heartbeats
            // every 50 iterations, which was ~3.7 s apart in the 147 s debug
            // run. 120 s is the same budget the vthread probes use and is
            // thirty times the worst gap measured.
            stall: Duration::from_secs(120),
            // Far above any measured run; the stall clock is the guard.
            ceiling: Duration::from_secs(1800),
        },
    );
    assert!(
        watched.stop == common::Stop::Exited,
        "{}",
        watched.diagnosis("threadpoolexecutor_prestart")
    );
    let output = watched.output;
    let _ = fs::remove_dir_all(&probe_classes);
    let combined = format!(
        "{}\n--- STDERR ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "prestart probe failed:\n{combined}"
    );
    assert!(
        combined.contains("PRESTART_OK iterations=2000 workers=2000"),
        "prestart probe did not complete all workers:\n{combined}"
    );
}
