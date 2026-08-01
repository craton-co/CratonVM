//! Regression coverage for the Tomcat endpoint-worker prestart flake.
//!
//! A real JDK `ThreadPoolExecutor` prestarts a newly constructed worker in
//! exactly the same way as Tomcat's endpoint executor. The probe must never
//! observe `IllegalThreadStateException` for that fresh worker.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn probe_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("resources")
        .join("cratonvm")
        .join("ThreadPoolExecutorPrestartProbe.java")
}

fn cratonvm_binary() -> Option<PathBuf> {
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
    let probe_classes = compile_probe().expect("compile prestart probe");

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
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match child.try_wait().expect("poll prestart probe") {
            Some(_) => break,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("ThreadPoolExecutor prestart probe timed out");
            }
        }
    }
    let output = child
        .wait_with_output()
        .expect("collect prestart probe output");
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
