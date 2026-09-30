// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gc-common w12-e: an uncaught exception in `main` still waits for the
//! non-daemon threads before the process exits with code 1
//! (`common-w11f-uncaught-exception-exit-abandons-non-daemon-threads`).
//!
//! The fixture `UncaughtKeeper.class` starts a non-daemon thread that sleeps
//! 300 ms and prints `KEEPER-DONE`, then throws from `main`. Measured on
//! HotSpot (Temurin 25.0.3+9, `java -cp . UncaughtKeeper`):
//!
//! ```text
//! Exception in thread "main" java.lang.IllegalStateException: main dies first
//!         at UncaughtKeeper.main(UncaughtKeeper.java:18)
//! KEEPER-DONE
//! rc=1
//! ```
//!
//! Before the fix CratonVM printed the trace and exited with 1 without
//! `KEEPER-DONE`: the launcher joined non-daemon threads only when `main`
//! returned.
//!
//! Bounded: the child is killed after 120 s and the test then fails, so a
//! wait that never ends cannot hang the test binary.

mod common;

use std::io::Read;
use std::time::{Duration, Instant};

#[test]
fn uncaught_exception_in_main_still_waits_for_a_non_daemon_thread() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    common::stage_class(tmp.path(), "UncaughtKeeper");

    let mut cmd = common::cratonvm_cmd();
    cmd.arg("--classpath")
        .arg(tmp.path())
        .arg("UncaughtKeeper")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().expect("spawn cratonvm");
    // Drain both pipes on their own threads so a full pipe cannot stall the
    // child while this thread polls it.
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out_pipe.read_to_string(&mut s);
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err_pipe.read_to_string(&mut s);
        s
    });

    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll cratonvm") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    let Some(status) = status else {
        panic!(
            "cratonvm did not exit within 120 s (the non-daemon thread ends after 300 ms); \
             stdout={stdout:?}, stderr={stderr:?}"
        );
    };

    assert_eq!(
        status.code(),
        Some(1),
        "an uncaught exception in main exits with code 1, as on HotSpot; \
         stdout={stdout:?}, stderr={stderr:?}"
    );
    assert!(
        stderr.to_lowercase().contains("illegalstateexception"),
        "the uncaught exception must still be rendered; stderr={stderr:?}"
    );
    assert!(
        stdout.contains("KEEPER-DONE"),
        "the non-daemon thread started by main must run to completion before the \
         process exits (HotSpot's DestroyJavaVM waits for it); \
         stdout={stdout:?}, stderr={stderr:?}"
    );
}
