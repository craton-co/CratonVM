// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Census and ratchet for the Java probe fixtures `vm/tests/*.rs` drives.
//!
//! # The defect this exists to make visible
//!
//! `.gitignore` line 12 is `apps/`. Every probe fixture written under that
//! directory was untracked, survived only in the working tree of whoever wrote
//! it, and vanished on the next clone — taking its test's ability to fail with
//! it. The referencing test then hits
//!
//! ```ignore
//! if !src.exists() { return; }        // cargo prints `ok` in 0.00 s
//! ```
//!
//! and reports a pass while asserting nothing. `common::require_fixture` (see
//! `vm/tests/common/mod.rs`) already makes that skip LOUD and promotes it to a
//! panic under `CRATONVM_REQUIRE_E2E`, and `probe_compile_guard.rs` already
//! requires every `apps/`-reaching test to route through it. Neither of those
//! **fails a default run**: the note goes to captured stderr that a green
//! `cargo test` summary never shows, and the gate variable is opt-in.
//!
//! So the absence stayed invisible for months, and the count kept growing. This
//! file is the missing third leg: a plain `cargo test --workspace` — which
//! `.github/workflows/ci.yml` runs on every push — now FAILS when the set of
//! absent fixtures changes in either direction.
//!
//! # Why a ratchet and not "every fixture must exist"
//!
//! 23 of the 27 fixtures below are absent today. Demanding all of them at once
//! would be a red that nobody can clear, and a permanently-red gate is a gate
//! nobody reads. A ratchet is falsifiable in both directions instead:
//!
//! * a fixture that is MISSING and not in [`MISSING_BASELINE`] fails — that is
//!   a fixture someone referenced without checking in, or one that has just
//!   disappeared from a fresh clone because it was never `git add -f`ed;
//! * a fixture that is PRESENT but still in [`MISSING_BASELINE`] also fails —
//!   restoring one is not finished until its row is deleted, which is what
//!   stops the baseline from silently outliving the problem.
//!
//! The second direction is the load-bearing one. A baseline that only ever
//! records "known bad" decays into a list of things nobody re-checks; this one
//! cannot, because clearing an entry is what makes the test go green again.
//!
//! # Placement
//!
//! `vm/tests/`, not `vm/src/vm/tests.rs`: that module is `synthetic-jdk`-only
//! and does not compile on the default build, so a test placed there goes dark.
//! This file needs no binary, no JDK and no fixture of its own — it only reads
//! the tree — so it runs everywhere `cargo test` runs.
//!
//! Background: W6-5-vacuous-tests.md §3.2, W7-51-vacuous-sweep-round-2.md.

use std::path::{Path, PathBuf};

/// One fixture: a stable id, the candidate paths (repo-relative, first match
/// wins — several tests accept a `probes/` copy as well as the `apps/` one),
/// and the `vm/tests/*.rs` files that drive it.
type Fixture = (
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
);

/// Every Java probe fixture referenced from `vm/tests/*.rs`.
///
/// Paths are repo-relative and are the paths the referencing test actually
/// builds — verified by reading each test, not by globbing the tree, because a
/// glob of `apps/` finds only what happens to be on this machine.
const FIXTURES: &[Fixture] = &[
    (
        "annotation_proxy_probe",
        &["apps/annotation_proxy_probe/AnnotationProxyProbe.java"],
        &["wp2_7_annotation_proxy.rs"],
    ),
    (
        "aqs_probe",
        &["apps/aqs_probe/AqsProbe.java"],
        &["cluster_a_aqs_chm.rs"],
    ),
    (
        "atomic_probe",
        &["apps/atomic_probe/AtomicProbe.java"],
        &["wave4_a_atomic.rs"],
    ),
    (
        "bc_probe",
        &["apps/bc_probe/BcProbe.java"],
        &["wave2_bc_probe.rs"],
    ),
    (
        "bigdecimal_probe",
        &["apps/bigdecimal_probe/BdProbe.java", "probes/BdProbe.java"],
        &["rbigdec1_arithmetic.rs", "rbigdec1_full_arithmetic.rs"],
    ),
    (
        "bytebuddy_probe",
        &["apps/bytebuddy_probe/ByteBuddyProbe.java"],
        &["wave2_bytebuddy.rs"],
    ),
    (
        "chm_basic",
        &["apps/chm_basic/ChmScale.java"],
        &["wave2_chm.rs"],
    ),
    (
        "cleaner_probe",
        &["apps/cleaner_probe/CleanerProbe.java"],
        &["wave2_cleaner.rs"],
    ),
    (
        "collection_tostring_probe",
        &["apps/collection_tostring_probe/CollProbe.java"],
        &["cluster_b_collection_tostring.rs"],
    ),
    (
        "console_probe",
        &["apps/console_probe/ConsoleProbe.java"],
        &["wave3_console_module.rs"],
    ),
    (
        "constructor_probe",
        &["apps/constructor_probe/ConstructorProbe.java"],
        &["cluster_c_constructor.rs", "wp2_6_constructor_edges.rs"],
    ),
    (
        "executor_probe",
        &["apps/executor_probe/ExecProbe.java"],
        &["wave1_c_executor.rs"],
    ),
    (
        "findspecial_probe",
        &["apps/findspecial_probe/FindSpecialProbe.java"],
        &["wp2_9_findspecial.rs"],
    ),
    (
        "fjp_probe",
        &["apps/fjp_probe/FjpProbe.java", "probes/FjpProbe.java"],
        &["fjp_recursive.rs", "rfjp1_recursive.rs"],
    ),
    ("h2", &["apps/h2/H2Test.java"], &["wave2_h2_connection.rs"]),
    (
        "jmx_probe",
        &["apps/jmx_probe/JmxProbe.java"],
        &["wave1_a_jmx_mxbeans.rs"],
    ),
    (
        "lm_subclass",
        &["apps/lm_subclass/LmSubclass.java"],
        &["block_2b_logmanager_factory.rs"],
    ),
    (
        "method_invoke_probe",
        &["apps/method_invoke_probe/MethodInvokeProbe.java"],
        &["wp2_2_method_invoke_matrix.rs"],
    ),
    (
        "methodhandles_probe",
        &["apps/methodhandles_probe/MhProbe.java"],
        &["wave2_c_methodhandles.rs"],
    ),
    (
        "proxy_probe",
        &["apps/proxy_probe/ProxyProbe.java"],
        &["wp2_5_proxy.rs"],
    ),
    (
        "reflect_probe",
        &["apps/reflect_probe/ReflectProbe.java"],
        &["wave2_a_class_getdeclared.rs", "wp2_1_reflect.rs"],
    ),
    (
        "scanner_probe",
        &["apps/scanner_probe/ScannerProbe.java"],
        &["wave3_scanner.rs"],
    ),
    (
        "selector_probe",
        &["apps/selector_probe/SelectorProbe.java"],
        &["wave3_c_selector.rs"],
    ),
    (
        "string_decode_probe/DeepListProbe",
        &["apps/string_decode_probe/DeepListProbe.java"],
        &["wave2_string_decode.rs"],
    ),
    (
        "string_decode_probe/IntListProbe",
        &["apps/string_decode_probe/IntListProbe.java"],
        &["wave2_string_decode.rs"],
    ),
    (
        "string_indexof_probe",
        &["apps/string_indexof_probe/SiProbe.java"],
        &["wave2_string_indexof.rs"],
    ),
    (
        "xml_probe",
        &["apps/xml_probe/XmlProbe.java"],
        &["wave1_d_xml_stax.rs"],
    ),
];

/// Fixtures absent from the tree right now, each with the reason it has not
/// been rebuilt. **Deleting a row is how a restoration is finished** — a
/// restored fixture whose row is still here fails this test.
///
/// Every entry is a test that currently asserts NOTHING on a default run.
const MISSING_BASELINE: &[(&str, &str)] = &[
    (
        "annotation_proxy_probe",
        "not rebuilt in W7-51; the annotation-proxy surface needs the historical \
         @Test/@Other/@TargetA stub set the harness names, which is not recoverable from \
         the assertions alone",
    ),
    (
        "aqs_probe",
        "not rebuilt in W7-51 — AQS handoff timing; the vector is a latency shape, and a \
         rebuilt one that does not reproduce the historical contention would be green-forever",
    ),
    ("bc_probe", "not rebuilt in W7-51"),
    (
        "bytebuddy_probe",
        "NOT RECONSTRUCTIBLE HERE: needs the Byte Buddy jar, a third-party artefact this \
         repository does not carry",
    ),
    ("cleaner_probe", "not rebuilt in W7-51"),
    ("collection_tostring_probe", "not rebuilt in W7-51"),
    (
        "constructor_probe",
        "not rebuilt in W7-51 — 11 nested types across two harnesses",
    ),
    ("findspecial_probe", "not rebuilt in W7-51"),
    (
        "h2",
        "NOT RECONSTRUCTIBLE HERE: needs the H2 database jar, a third-party artefact this \
         repository does not carry",
    ),
    (
        "method_invoke_probe",
        "not rebuilt in W7-51 — 6 nested types",
    ),
    (
        "reflect_probe",
        "not rebuilt in W7-51 — 5 nested types across two harnesses",
    ),
    (
        "scanner_probe",
        "not rebuilt in W7-51. NOTE: W6-5 §3.2 recorded this one as \"self-generates its \
         source — likely OK\". It does not: `ensure_scanner_probe_compiled` reads \
         `ScannerProbe.java` off disk and skips when it is absent.",
    ),
    ("string_decode_probe/DeepListProbe", "not rebuilt in W7-51"),
    ("string_decode_probe/IntListProbe", "not rebuilt in W7-51"),
    ("string_indexof_probe", "not rebuilt in W7-51"),
    (
        "xml_probe",
        "not rebuilt in W7-51. NOTE: W6-5 §3.2 recorded this one as \"self-generates its \
         source — likely OK\". It does not: `write_fixture()` writes the XML *data* to \
         /tmp/test.xml; `XmlProbe.java` itself must be on disk, and the test skips when it \
         is not.",
    ),
];

/// Files that build a path under `apps/` for something that is NOT a probe
/// fixture this repository can carry, with the reason. Keeps
/// [`the_census_covers_every_test_that_reaches_into_apps`] honest without
/// pretending a multi-hundred-megabyte third-party distribution is a fixture.
const NOT_A_FIXTURE: &[(&str, &str)] = &[
    (
        "probe_compile_guard.rs",
        "the source-level guard — its `apps/` literals are its detector, not a lookup",
    ),
    (
        "wave3_spring_boot_fatjar.rs",
        "apps/spring-boot-suite-runner/insurance-backend.jar — a built application jar, \
         located via CRATONVM_SPRING_BOOT_FATJAR",
    ),
    (
        "wildfly_jboss_module_service_leak.rs",
        "apps/wildfly-dist/wildfly-32.0.1.Final — a third-party server distribution",
    ),
    ("probe_fixture_census.rs", "this census itself"),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm/ has a parent")
        .to_path_buf()
}

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// First candidate that exists, if any.
fn resolve(candidates: &[&str]) -> Option<PathBuf> {
    let root = workspace_root();
    candidates
        .iter()
        .map(|rel| root.join(rel))
        .find(|p| p.exists())
}

/// The ratchet. See this file's module docs for why it is falsifiable in both
/// directions.
#[test]
fn every_referenced_probe_fixture_is_present_or_baselined() {
    let mut vanished: Vec<String> = Vec::new();
    let mut restored: Vec<String> = Vec::new();

    for (id, candidates, owners) in FIXTURES {
        let baselined = MISSING_BASELINE.iter().any(|(b, _)| b == id);
        match resolve(candidates) {
            Some(_) if baselined => restored.push(format!(
                "{id} — now present, but still listed in MISSING_BASELINE"
            )),
            None if !baselined => vanished.push(format!(
                "{id}\n      searched: {}\n      driven by: {}",
                candidates.join(", "),
                owners.join(", ")
            )),
            _ => {}
        }
    }

    assert!(
        vanished.is_empty(),
        "[probe-fixture-census] {} checked-in probe fixture(s) are MISSING and are not in \
         MISSING_BASELINE:\n    {}\n\nThe test(s) driving them do not fail when the fixture is \
         absent — they return early and report `ok` in 0.00 s while asserting nothing.\n\nThe \
         usual cause is `.gitignore` line 12 (`apps/`): a fixture written there is UNTRACKED, so \
         it exists for its author and for nobody else. `git status` will not show it and \
         `git add -A` will not stage it. Either `git add -f` the fixture, or put it under \
         `probes/` (tracked; only `probes/*.class` and `probes/out/` are ignored) and add that \
         path to the fixture's candidate list here.\n\nIf a fixture is genuinely gone and cannot \
         be rebuilt now, add a row to MISSING_BASELINE saying so — do NOT delete the test.",
        vanished.len(),
        vanished.join("\n    ")
    );

    assert!(
        restored.is_empty(),
        "[probe-fixture-census] {} fixture(s) are present but still listed in \
         MISSING_BASELINE:\n    {}\n\nDelete their rows. A baseline that outlives the problem it \
         records is how a measured population turns back into an unmeasured one.",
        restored.len(),
        restored.join("\n    ")
    );
}

/// Detector liveness, in the shape `probe_compile_guard.rs` uses: if the census
/// stops describing the tree it becomes a test that cannot fail.
#[test]
fn the_census_names_real_tests_and_is_not_empty() {
    assert!(
        FIXTURES.len() >= 20,
        "[probe-fixture-census] the fixture table has shrunk to {} entries; either the probe \
         suite really was deleted, or this census has gone stale and is no longer measuring \
         anything. Do not lower this bound to make it pass.",
        FIXTURES.len()
    );

    let dir = tests_dir();
    let mut missing_owners: Vec<String> = Vec::new();
    for (id, _, owners) in FIXTURES {
        for owner in *owners {
            if !dir.join(owner).exists() {
                missing_owners.push(format!("{id} -> {owner}"));
            }
        }
    }
    assert!(
        missing_owners.is_empty(),
        "[probe-fixture-census] the census names test file(s) that no longer exist:\n  {}\n\nA \
         renamed or deleted harness leaves a census row measuring nobody. Update the row.",
        missing_owners.join("\n  ")
    );

    let mut dead_rows: Vec<&str> = Vec::new();
    for (id, _) in MISSING_BASELINE {
        if !FIXTURES.iter().any(|(f, _, _)| f == id) {
            dead_rows.push(id);
        }
    }
    assert!(
        dead_rows.is_empty(),
        "[probe-fixture-census] MISSING_BASELINE names fixture id(s) the census does not \
         know:\n  {}\n\nA baseline row for a fixture nobody references is dead weight that makes \
         the number look worse than it is.",
        dead_rows.join("\n  ")
    );
}

/// Every `vm/tests/*.rs` that reaches into the gitignored `apps/` tree must be
/// accounted for — as the owner of a census row, or in [`NOT_A_FIXTURE`].
///
/// Without this, adding a new test with a new untracked fixture would grow the
/// silent-skip population without moving any number here. The needles are the
/// same ones `probe_compile_guard.rs` uses.
#[test]
fn the_census_covers_every_test_that_reaches_into_apps() {
    let needles = [
        "join(\"apps\")",
        "join(\"apps/",
        "join(\"apps\\\\",
        "PathBuf::from(\"apps",
        "Path::new(\"apps",
    ];

    let dir = tests_dir();
    let mut uncovered: Vec<String> = Vec::new();
    let mut matched = 0usize;

    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(Result::ok);
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        if !needles.iter().any(|n| src.contains(n)) {
            continue;
        }
        matched += 1;
        let owned = FIXTURES
            .iter()
            .any(|(_, _, owners)| owners.contains(&name.as_str()));
        let exempt = NOT_A_FIXTURE.iter().any(|(f, _)| *f == name);
        if !owned && !exempt {
            uncovered.push(name);
        }
    }

    assert!(
        matched >= 20,
        "[probe-fixture-census] only {matched} test file(s) matched the `apps/` needles; the \
         detector has gone stale (a shared path helper, a renamed fixture root) and this check is \
         no longer covering anything. Fix the needles, do not lower this bound."
    );

    assert!(
        uncovered.is_empty(),
        "[probe-fixture-census] {} test file(s) build a path into the gitignored `apps/` tree but \
         appear in neither FIXTURES nor NOT_A_FIXTURE:\n  {}\n\nAdd the fixture they need to \
         FIXTURES (and to MISSING_BASELINE if it is not in the tree yet), so a new untracked \
         fixture cannot grow the silent-skip population without moving a number here.",
        uncovered.len(),
        uncovered.join("\n  ")
    );
}
