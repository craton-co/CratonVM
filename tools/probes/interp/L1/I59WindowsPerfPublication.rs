// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
// Standalone Windows publisher fixture: cross-compile with rustc from the
// Linux host, then run i59-windows-perf-gate.py with the Windows JDK in Wine.
#[path = "../../../../vm/src/runtime/perf_data_windows.rs"]
mod windows;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args[1] == "absent" {
        assert!(!std::path::Path::new(&args[2]).exists(), "file survived process termination");
        println!("ABSENT");
        return;
    }
    let bytes = std::fs::read(&args[1]).unwrap();
    let (path, lease) = windows::publish(std::process::id(), &bytes).expect("publish");
    println!("READY {} {}", std::process::id(), path.display());
    assert!(
        windows::publish(std::process::id(), &bytes).is_none(),
        "another VM overwrote publication"
    );
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
    drop(lease);
    assert!(!path.exists(), "file survived last handle close");
    println!("REMOVED");
}
