//! Real-JDK regression for URLClassLoader's parent-first `getResource` contract.

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    std::env::var_os("CRATONVM_BIN")
        .map(PathBuf::from)
        .filter(|path| path.exists())
}

fn java_home() -> Option<PathBuf> {
    std::env::var_os("CRATONVM_JAVA_HOME")
        .or_else(|| std::env::var_os("JAVA_HOME"))
        .map(PathBuf::from)
        .filter(|path| path.exists())
}

fn compiled_classes() -> Option<PathBuf> {
    option_env!("CRATONVM_TEST_CLASSES_DIR")
        .map(PathBuf::from)
        .filter(|path| {
            path.join("cratonvm/UrlClassLoaderResourceDelegation.class")
                .exists()
        })
}

#[test]
fn url_classloader_child_get_resource_delegates_to_parent_with_and_without_jit() {
    let Some(binary) = cratonvm_binary() else {
        eprintln!("[url-classloader-parent-resource] CRATONVM_BIN is not set; skipping");
        return;
    };
    let Some(classes) = compiled_classes() else {
        panic!("URLClassLoader resource regression fixture was not compiled");
    };

    for nojit in [false, true] {
        let mut command = Command::new(&binary);
        if let Some(home) = java_home() {
            command.arg("--java-home").arg(home);
        }
        if nojit {
            command.arg("--nojit");
        }
        let output = command
            .arg("-cp")
            .arg(&classes)
            .arg("cratonvm.UrlClassLoaderResourceDelegation")
            .output()
            .expect("failed to launch CratonVM URLClassLoader resource regression");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("URL_CLASSLOADER_PARENT_RESOURCE_OK"),
            "URLClassLoader parent-resource regression failed (nojit={nojit}). stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}
