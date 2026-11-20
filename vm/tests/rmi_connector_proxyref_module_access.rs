// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! `RMIConnector.<clinit>` defines `jdk.jmx.remote.internal.rmi.PRef` (a subclass
//! of `java.management.rmi`'s encapsulated `ProxyRef`) in a dynamic, layerless
//! module `jdk.remoteref` and exports `ProxyRef`'s package to it. The class must
//! be judged a member of that module, not of its loader's unnamed module, or the
//! first `JMXConnectorFactory.connect("service:jmx:rmi:...")` dies with
//! `IllegalAccessError: superclass access check failed` (Tomcat
//! `TestJMXAccessorTask`). Runs the `cratonvm` binary; skips when it is absent.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod common;

fn binary() -> Option<PathBuf> {
    let found = std::env::var("CRATONVM_BIN")
        .map(PathBuf::from)
        .ok()
        .filter(|p| p.exists())
        .or_else(|| {
            let target = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.join("target");
            let exe = if cfg!(windows) { "cratonvm.exe" } else { "cratonvm" };
            ["release", "debug"]
                .iter()
                .map(|p| target.join(p).join(exe))
                .find(|c| c.exists())
        });
    common::require_binary(found)
}

#[test]
fn rmi_connector_defines_its_proxyref_subclass() {
    let Some(bin) = binary() else {
        eprintln!("[rmi-proxyref] skipping — cratonvm binary not available");
        return;
    };
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/resources/rmiprobe");
    let mut cmd = Command::new(bin);
    cmd.arg("--real-jdk").arg("-cp").arg(&dir).arg("RmiProbe");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let out = common::wait_draining(cmd.spawn().expect("spawn cratonvm"), Duration::from_secs(120)).output;
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        stdout.contains("RMI-CONNECT: io-exception"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
