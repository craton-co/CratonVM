// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 9 wave 9, lane `review9b`: the monomorphic `invokevirtual` fast door
//! (`dispatch_virtual.rs::execute_invokevirtual_fast_door`) must hold an
//! `ACC_SYNCHRONIZED` callee's monitor on every route it serves.
//!
//! The door admits synchronized callees (`invoke_fast::door_sync_enabled`,
//! default on) and takes the monitor at its verbatim frame push. Two earlier
//! exits in the same function skipped it:
//!
//! * **F1** — the compiled-callee arm. When `execute_jit_call_decoded` DECLINES
//!   (`Ok(None)`: more arguments than the register-only JIT ABI carries, or a
//!   deopt with no resumable frame) the door re-ran the callee interpreted with
//!   `monitor_obj = None`. `SyncMonitorProbe wide` (a synchronized instance
//!   method with 8 `int` parameters, 4 threads) lost updates on w8b:
//!   1_195_622 of 1_200_000. `CRATONVM_JIT_NO_DOOR_SYNC=1` made it exact.
//! * **F2** — the trivial-getter shortcut answered a `synchronized` getter
//!   from the quickened field site, without a frame and so without the lock.
//!   `SyncMonitorProbe getter` printed `seen=1` (the value written while
//!   another thread held the monitor) on w8b, `seen=2` with the door off.
//!
//! Both run as a child `cratonvm` process on a `javac`-compiled probe, in the
//! style of `synchronized_wrapper_mutex.rs`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

const PROBE_SRC: &str = r#"
public class SyncMonitorProbe {
    int count;
    int sink;
    int x;

    // 8 params + receiver: wider than the register-only JIT ABI, so a compiled
    // body is always DECLINED by the decoded door and re-run interpreted.
    synchronized void bump(int a, int b, int c, int d, int e, int f, int g, int h) {
        int v = count;
        for (int i = 0; i < (a & 3); i++) sink += i ^ b ^ c ^ d ^ e ^ f ^ g ^ h;
        count = v + 1;
    }

    synchronized int getX() { return x; }

    static int read(SyncMonitorProbe o) { return o.getX(); }

    static void wide() throws Exception {
        final SyncMonitorProbe s = new SyncMonitorProbe();
        final int n = 300_000;
        Runnable r = () -> {
            for (int i = 0; i < n; i++) s.bump(i, 1, 2, 3, 4, 5, 6, 7);
        };
        Thread[] ts = new Thread[4];
        for (int i = 0; i < ts.length; i++) { ts[i] = new Thread(r); ts[i].start(); }
        for (Thread t : ts) t.join();
        System.out.println("WIDE count=" + s.count + " expected=" + (4 * n));
    }

    static void getter() throws Exception {
        final SyncMonitorProbe o1 = new SyncMonitorProbe();
        final SyncMonitorProbe o2 = new SyncMonitorProbe();
        final java.util.concurrent.CountDownLatch held = new java.util.concurrent.CountDownLatch(1);
        Thread a = new Thread(() -> {
            synchronized (o1) {
                o1.x = 1;
                held.countDown();
                try { Thread.sleep(1500); } catch (InterruptedException e) {}
                o1.x = 2;
            }
        });
        a.start();
        held.await();
        // Warm the site after all class loading (a class-definition epoch bump
        // drops the quickened field sites), on an unlocked receiver of the
        // same class, so the next call is a warm monomorphic door hit.
        int s = 0;
        for (int i = 0; i < 300; i++) s += read(o2);
        int seen = read(o1); // must block until A releases o1, then read 2
        a.join();
        System.out.println("GETTER seen=" + seen + " s=" + s);
    }

    public static void main(String[] args) throws Exception {
        if (args[0].equals("wide")) wide(); else getter();
    }
}
"#;

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
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = manifest.parent()?.join("target").join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn javac_bin() -> PathBuf {
    let exe = if cfg!(windows) { "javac.exe" } else { "javac" };
    if let Ok(home) = std::env::var("CRATONVM_TEST_JAVA_HOME") {
        let candidate = PathBuf::from(home).join("bin").join(exe);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(exe)
}

/// Compile the probe into a fresh temp dir; `None` only when `javac` cannot be
/// launched at all.
fn compile_probe() -> Option<tempfile::TempDir> {
    let temp = tempfile::tempdir().expect("temp probe dir");
    let source = temp.path().join("SyncMonitorProbe.java");
    std::fs::File::create(&source)
        .and_then(|mut f| f.write_all(PROBE_SRC.trim_start().as_bytes()))
        .expect("write probe source");
    match Command::new(javac_bin())
        .arg("-d")
        .arg(temp.path())
        .arg(&source)
        .output()
    {
        Err(e) => {
            eprintln!("[r9w9_review9b] javac could not be executed: {e}; skipping");
            None
        }
        Ok(o) => {
            assert!(
                o.status.success(),
                "[r9w9_review9b] the embedded probe failed to compile — fix the probe source. \
                 javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            Some(temp)
        }
    }
}

fn run_probe(bin: &Path, dir: &Path, mode: &str) -> String {
    let mut child = Command::new(bin)
        .arg("-cp")
        .arg(dir)
        .arg("SyncMonitorProbe")
        .arg(mode)
        .env("CRATONVM_DISABLE_DEFAULT_WATCHDOG", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn cratonvm");
    // `common::wait_draining`, not a `try_wait` poll loop. The loop this
    // replaces read NEITHER pipe until the child had exited, and a piped stream
    // nobody reads blocks its WRITER once the OS buffer fills -- so a probe run
    // under a verbose `CRATONVM_DBG*` flag stalls inside its own exit path and
    // the loop reports "timed out", which reads like the VM hanging rather than
    // like the harness holding it.
    //
    // `wait_draining`'s own doc comment says this shape is "everywhere in
    // `vm/tests`". It was written after `class_loader_unload_regression` lost a
    // session to it (stdout 124 bytes, stderr 101,808) and its closing sentence
    // is the rule: "a test must not depend on the process it drives staying
    // under 64 KiB". Measured on Windows, 2026-09-22: four files carrying this
    // loop failed one `cargo test --workspace` run together, and not one of the
    // failures was about the thing its test names.
    let timeout = Duration::from_secs(180);
    let finished = common::wait_draining(child, timeout);
    assert!(
        !finished.timed_out,
        "SyncMonitorProbe {mode} timed out after {timeout:?}"
    );
    let output = finished.output;
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// F1: the door's compiled-callee decline re-runs a synchronized callee
/// interpreted, and that run must hold the receiver's monitor.
#[test]
fn a_declined_compiled_synchronized_callee_reruns_under_its_monitor() {
    let Some(bin) = cratonvm_binary() else {
        return;
    };
    let Some(dir) = compile_probe() else {
        return;
    };
    let out = run_probe(&bin, dir.path(), "wide");
    assert!(
        out.contains("WIDE count=1200000 expected=1200000"),
        "a synchronized 8-int instance method lost updates under 4 threads — the \
         invoke fast door ran it without its monitor:\n{out}"
    );
}

/// F2: the door's trivial-getter shortcut must not answer a synchronized
/// getter without taking the monitor.
#[test]
fn a_synchronized_trivial_getter_blocks_on_a_held_monitor() {
    let Some(bin) = cratonvm_binary() else {
        return;
    };
    let Some(dir) = compile_probe() else {
        return;
    };
    let out = run_probe(&bin, dir.path(), "getter");
    assert!(
        out.contains("GETTER seen=2 "),
        "a synchronized getter returned the value written INSIDE another thread's \
         critical section — the invoke fast door answered it lock-free:\n{out}"
    );
}
