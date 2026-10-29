// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The self-lock census as a standing guard on the synchronized probes (JIT
//! round 19 wave 7, lane handler7, SELFLOCK19-4 of
//! `docs/feature-designs/jit-ir-optimizations.md`).
//!
//! Drives the checked-in probe `tools/probes/R19Sync5Nest.java` (the round-19
//! wave-5 sync probe: nested `synchronized` blocks inside `synchronized`
//! methods, an inner block that throws every 64th call through the method to
//! its caller, a catch inside the outer block, recursive self-calls with a
//! table, a static synchronized callee loop, `wait`/`notify` and a contended
//! run) under `CRATONVM_DBG=jit-method-stats`, in both policies (the default
//! `--jdk-only` and `--compatible`), and asserts on the exit lines the VM
//! prints:
//!
//! * `drop-unreleased-frames=0`: no drop site discarded a frame naming a
//!   compiled lock hold without releasing it (the round-18 flip condition,
//!   now a regression guard);
//! * the positive control (r22 w10b): `exc-handover-bodies>0`,
//!   `hold-frames-taken>0` and a counted release (`callee-claim-handovers` +
//!   `door-drain-handovers` + `drop-released-frames` > 0): compiled
//!   self-locking bodies DID hand their monitor over into exceptional frames
//!   and those frames reached a release path, so the zero above is a
//!   statement about real compiled holds and not about unreached code. The
//!   old control (`drop-released-frames>0`) is unreachable from Java at
//!   defaults (the claim and the door drain consume every handed frame first;
//!   page r22-w7-selflock-census-positive-control-unreached-20261020); the
//!   drop path keeps a unit test in `deopt_resume.rs`. The probe's
//!   `table-miss` fact drives the hand-over;
//! * `body-published>0`: self-locking bodies were compiled at all (without
//!   that every row above is vacuous);
//! * `monitors-held-at-thread-exit=0` wherever it is printed (SELFLOCK19-3's
//!   end-to-end leak check: an inflated monitor a terminating thread still
//!   owned);
//! * no `deopt stash residual` warning: `dropped_unclaimed=0`. That counter is
//!   a SUPERSET of SELFLOCK19-2's population (a jit-internal stash discard
//!   that drops a frame naming a compiled hold: `deopt.rs` `pop_stash_entry`
//!   and the stash depth cap both count every frame they drop there), so its
//!   zero here closes SELFLOCK19-2 on these shapes without a new counter.
//!
//! The baselines were taken by reading, not by a run of this file (the lane
//! that wrote it builds no VM): the orchestrator's first run with
//! `CRATONVM_REQUIRE_E2E=1` is the baseline, and a positive-control failure
//! there means the probe does not reach the drop sites on that build (pick
//! the shape that does, not a weaker assertion).
//!
//! Skips (loudly) without a binary, a JDK or the fixture; under
//! `CRATONVM_REQUIRE_E2E=1` each of those is a failure instead.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;

const TAG: &str = "jit_selflock_census_ratchet";
const PROBE: &str = "R19Sync5Nest";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm/ has a parent")
        .to_path_buf()
}

fn probe_source() -> Option<PathBuf> {
    common::require_fixture(
        TAG,
        "the R19Sync5Nest probe (tools/probes/R19Sync5Nest.java)",
        &[workspace_root()
            .join("tools")
            .join("probes")
            .join("R19Sync5Nest.java")],
    )
}

fn cratonvm_binary() -> Option<PathBuf> {
    let found = std::env::var("CRATONVM_BIN")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .or_else(|| {
            let exe = if cfg!(windows) {
                "cratonvm.exe"
            } else {
                "cratonvm"
            };
            ["release", "debug"]
                .iter()
                .map(|profile| workspace_root().join("target").join(profile).join(exe))
                .find(|p| p.exists())
        });
    common::require_binary(found)
}

fn jdk_home() -> Option<PathBuf> {
    let found = ["CRATONVM_TEST_JDK", "JAVA_HOME"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .map(PathBuf::from)
        .find(|p| p.exists());
    common::require_jdk(found)
}

fn compile_probe(javac: &Path, src: &Path) -> Option<PathBuf> {
    let dir = workspace_root().join("target").join(TAG);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::remove_file(dir.join(format!("{PROBE}.class")));
    let out = match Command::new(javac).arg("-d").arg(&dir).arg(src).output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[{TAG}] javac could not be executed: {e}; skipping");
            return None;
        }
    };
    assert!(
        out.status.success() && dir.join(format!("{PROBE}.class")).exists(),
        "[{TAG}] the probe failed to compile — fix tools/probes/{PROBE}.java. javac stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(dir)
}

fn run_probe(bin: &Path, jdk: &Path, classes: &Path, policy: Option<&str>) -> (String, String) {
    let mut cmd = Command::new(bin);
    if let Some(flag) = policy {
        cmd.arg(flag);
    }
    cmd.arg("--java-home")
        .arg(jdk)
        .env("CRATONVM_DBG", "jit-method-stats")
        .arg("-c")
        .arg(classes)
        .arg(PROBE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("[{TAG}] could not spawn cratonvm: {e}"));
    let timed = common::wait_draining(child, Duration::from_secs(900));
    let stdout = String::from_utf8_lossy(&timed.output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&timed.output.stderr).into_owned();
    assert!(
        !timed.timed_out,
        "[{TAG}] {policy:?}: the probe did not finish within the cap.\nstdout:\n{stdout}\nstderr tail:\n{}",
        tail(&stderr)
    );
    (stdout, stderr)
}

/// `name=value` out of the `[cratonvm] self-lock census:` line.
fn census_row(census: &str, name: &str) -> u64 {
    let key = format!("{name}=");
    census
        .split_whitespace()
        .find_map(|w| w.strip_prefix(key.as_str()))
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("[{TAG}] the census line has no `{name}` row: {census}"))
}

fn tail(text: &str) -> String {
    const KEEP: usize = 4000;
    if text.len() <= KEEP {
        return text.to_string();
    }
    let mut cut = text.len() - KEEP;
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!("… (truncated)\n{}", &text[cut..])
}

fn check_arm(bin: &Path, jdk: &Path, classes: &Path, policy: Option<&str>) {
    let (stdout, stderr) = run_probe(bin, jdk, classes, policy);
    assert!(
        stdout.contains("fact nest-contended"),
        "[{TAG}] {policy:?}: the probe did not run to completion.\nstdout:\n{stdout}\nstderr tail:\n{}",
        tail(&stderr)
    );
    // JIT round 22 wave 12 (lane review): the hand-over kernel's own Java-visible
    // answer. The census rows below say that compiled holds were handed over
    // and reached a release path; this says the release was RIGHT: every call
    // counted, every NPE caught where HotSpot catches it, and the receiver's
    // monitor neither held by this thread nor busy for another one afterwards
    // (`held=false free=true`). `monitors-held-at-thread-exit` cannot see this
    // kernel, which runs on the main thread. The tokens are arithmetic of the
    // driver (4 x 30 000 calls, a null every 64th) and HotSpot 25's line
    // (`tools/probes/R19Sync5Nest.java` header); `check=` is left out, as on
    // the page.
    let table_miss = stdout
        .lines()
        .find(|l| l.starts_with("fact table-miss "))
        .unwrap_or_else(|| {
            panic!("[{TAG}] {policy:?}: the probe printed no `fact table-miss` line.\nstdout:\n{stdout}")
        });
    for want in ["ok=118128", "caught=1872", "acc=120000", "held=false", "free=true"] {
        assert!(
            table_miss.split_whitespace().any(|w| w == want),
            "[{TAG}] {policy:?}: `fact table-miss` differs from HotSpot (`{want}` missing): {table_miss}"
        );
    }
    let census = stderr
        .lines()
        .find(|l| l.contains("[cratonvm] self-lock census:"))
        .unwrap_or_else(|| {
            panic!(
                "[{TAG}] {policy:?}: no self-lock census line under CRATONVM_DBG=jit-method-stats \
                 (no synchronized method was compiled?).\nstderr tail:\n{}",
                tail(&stderr)
            )
        });
    assert!(
        census_row(census, "body-published") > 0,
        "[{TAG}] {policy:?}: no self-locking body was published, so every row below is vacuous: {census}"
    );
    assert_eq!(
        census_row(census, "drop-unreleased-frames"),
        0,
        "[{TAG}] {policy:?}: a drop site discarded a frame naming a compiled hold without releasing \
         it (rerun with CRATONVM_DBG_DEOPT=1 for the site): {census}"
    );
    // Positive control (r22 w10b). The old control required
    // `drop-released-frames > 0`, but at defaults that row is unreachable from
    // Java: every exceptional hand-over frame a compiled self-locking body
    // stashes is consumed by the callee-handler claim
    // (`precise_handler_frame_for`) or the door drain
    // (`route_jit_signal_exception`) before any compiled catch's floor walk
    // can see it as an orphan (12 kernels, w8-w10; page
    // r22-w7-selflock-census-positive-control-unreached-20261020). The control
    // asserts the same fact through the rows that do move: compiled code
    // published hand-over bodies, took holds into handed frames, and those
    // frames reached a counted release path (a leak among them still shows as
    // `drop-unreleased-frames` or `monitors-held-at-thread-exit`). The orphan drop itself is
    // covered by `deopt_resume.rs`
    // `a_dropped_frame_with_a_compiled_hold_moves_both_drop_census_rows`.
    assert!(
        census_row(census, "exc-handover-bodies") > 0,
        "[{TAG}] {policy:?}: positive control: no self-locking body with an exception table was \
         published with the exceptional hand-over, so the leak checks prove nothing: {census}"
    );
    let taken = census_row(census, "hold-frames-taken");
    let released = census_row(census, "callee-claim-handovers")
        + census_row(census, "door-drain-handovers")
        + census_row(census, "drop-released-frames");
    assert!(
        taken > 0,
        "[{TAG}] {policy:?}: positive control: no handed frame named a compiled hold, so \
         `drop-unreleased-frames=0` proves nothing: {census}"
    );
    assert!(
        released > 0,
        "[{TAG}] {policy:?}: positive control: compiled holds were taken but no handed frame \
         reached a counted release path (claim, door drain or drop): {census}"
    );
    for line in stderr.lines() {
        if let Some(n) = line
            .split_whitespace()
            .find_map(|w| w.strip_prefix("monitors-held-at-thread-exit="))
            .and_then(|v| v.parse::<u64>().ok())
        {
            assert_eq!(
                n, 0,
                "[{TAG}] {policy:?}: a terminating thread still owned a monitor: {line}"
            );
        }
    }
    assert!(
        !stderr.contains("deopt stash residual"),
        "[{TAG}] {policy:?}: a stashed frame was discarded unclaimed (SELFLOCK19-2's population is a \
         subset of these): {}",
        stderr
            .lines()
            .find(|l| l.contains("deopt stash residual"))
            .unwrap_or("")
    );
}

#[test]
fn the_sync_probe_leaks_no_compiled_hold_and_reaches_the_drop_sites() {
    let Some(bin) = cratonvm_binary() else {
        eprintln!("[{TAG}] no cratonvm binary; skipping");
        return;
    };
    let Some(jdk) = jdk_home() else {
        eprintln!("[{TAG}] no JDK; skipping");
        return;
    };
    let Some(src) = probe_source() else { return };
    let javac = jdk
        .join("bin")
        .join(if cfg!(windows) { "javac.exe" } else { "javac" });
    let Some(classes) = compile_probe(&javac, &src) else {
        return;
    };
    check_arm(&bin, &jdk, &classes, None);
    check_arm(&bin, &jdk, &classes, Some("--compatible"));
}
