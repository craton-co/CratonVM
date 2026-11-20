// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn compatible_version_initializes_flags_after_boot_cache_lookup() {
    let home = tempfile::tempdir().unwrap();
    for mode in ["--compatible", "--fast-boot"] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_cratonvm"))
            .args([mode, "-version"])
            .env("HOME", home.path())
            .env_remove("XDG_CACHE_HOME")
            .env_remove("CRATONVM_BOOT_IMAGE")
            .env_remove("CRATONVM_BOOT_IMAGE_DIR")
            .output()
            .expect("launcher");
        assert!(output.status.success(), "{mode}: {}", String::from_utf8_lossy(&output.stderr));
    }
}
