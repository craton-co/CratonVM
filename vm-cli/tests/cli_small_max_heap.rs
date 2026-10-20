// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot rejects these heaps before class loading or version-boot work.
#[test]
fn too_small_maximum_heap_is_a_reported_startup_failure_in_both_policies() {
    let mut collectors = vec!["-XX:+UseG1GC", "-XX:+UseGenerationalGC"];
    #[cfg(feature = "zgc")]
    collectors.push("-XX:+UseZGC");
    for policy in ["--jdk-only", "--compatible"] {
        for &gc in &collectors {
            for size in ["0", "1", "128k", "1m", "2097151"] {
                for max_heap in [format!("-Xmx{size}"), format!("-XX:MaxHeapSize={size}")] {
                    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cratonvm"))
                        .args([policy, gc, &max_heap, "-version"])
                        .output()
                        .expect("launcher");
                    assert_eq!(output.status.code(), Some(1), "{policy} {gc} {size}");
                    if size == "0" {
                        assert!(output.stdout.is_empty());
                        assert_eq!(String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
                            format!("Invalid maximum heap size: {max_heap}\nError: Could not create the Java Virtual Machine.\nError: A fatal exception has occurred. Program will exit.\n"));
                    } else {
                        assert_eq!(
                            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
                            "Error occurred during initialization of VM\nToo small maximum heap\n"
                        );
                        assert!(
                            output.stderr.is_empty(),
                            "{}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                }
            }
        }
    }
}
