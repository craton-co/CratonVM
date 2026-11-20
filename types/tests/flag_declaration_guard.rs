// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! A new `CRATONVM_*` variable cannot appear without being declared.
//!
//! # What this catches that the other two guards do not
//!
//! * `flag_surface.rs` pins [`INVENTORY`] against the checked-in fixture. Both
//!   sides are files a person edits, so it only notices a *declaration* that
//!   went missing — never a *read site* that was never declared.
//! * `flag_env_mutation_guard.rs` catches `set_var` of an already-declared
//!   name.
//! * `tools/flag-census/check-surface.sh` does scan for literals, but it is a
//!   shell script in CI, it only looks at `<crate>/src`, and at the time this
//!   test was written it was red: **63** variables were read by production code
//!   and declared nowhere. That is the failure mode this test exists for, and a
//!   `cargo test` gate is much harder to not-run than a CI shell step.
//!
//! # Why an undeclared flag is a bug, not untidiness
//!
//! `cratonvm_types::flags::runtime_var` serves *declared* names from the one
//! immutable snapshot and falls through to `std::env` for everything else. So a
//! flag's declaration status silently changes its semantics:
//!
//! | | declared | undeclared |
//! |---|---|---|
//! | source of the value | the latched snapshot | live `getenv` |
//! | reachable from `CRATONVM_<GROUP>=token` | yes | no |
//! | visible to `with_thread_overrides` | yes | **no** |
//! | listed in `docs/CONFIG.md` | yes | no |
//!
//! The third row is the one that bites. A test that arranges a flag through the
//! supported override hook has no effect on an undeclared flag, so the flag
//! keeps whatever the developer's ambient environment says — the test passes or
//! fails for a reason unrelated to what it claims to check. The mirror-image
//! defect (`set_var` on a *declared* flag) is written up in
//! `libcratonvm-no-jdk-test-order-dependent-fixed-20260730.md`.
//!
//! # Precision
//!
//! Only an **exact whole-string literal** counts: the closing quote must
//! immediately follow the name. That is what keeps this from firing on
//! `b"CRATONVM_MODULES_MARKER\n"` (a file's contents), on
//! `"[CRATONVM_STREAM_PIN_CANARY] {site}: …"` (a log tag), and on
//! `"CRATONVM_DBG=jit-method-stats"` (a message telling the operator what to
//! export). Whole-line comments are skipped as well, so the hundreds of
//! `/// CRATONVM_X=1 does …` doc comments in this tree cost nothing.
//!
//! Known blind spot, stated rather than papered over: a name assembled at
//! runtime (`format!("CRATONVM_REAL_{sub}")`) cannot be judged statically. The
//! same caveat applies to `flag_env_mutation_guard.rs`.

use cratonvm_types::flag_groups::{Group, INVENTORY, SCALARS};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Directory names never scanned: build output, VCS metadata, and the Java
/// application harnesses under `apps/` (not Rust sources).
const SKIPPED_DIRS: &[&str] = &["target", ".git", "apps", "node_modules", "docs"];

/// Names that may appear as a literal without being declared, each with the
/// reason it is exempt.
///
/// Every row is one of three kinds, and nothing else belongs here:
///
/// 1. **Not an environment variable at all** — a name that happens to match the
///    pattern while being an ABI constant or an assertion needle.
/// 2. **A deliberate "this variable does not exist" probe** in a unit test.
/// 3. **A test-harness or build-script knob** that no production code reads.
///    These are read before or outside a VM, where the snapshot does not exist
///    and live `std::env` semantics are the correct ones. Declaring them would
///    put harness plumbing into `docs/CONFIG.md` and into `CRATONVM_TEST=…`,
///    which is the surface growth this whole exercise is undoing.
/// 4. **Part of the configuration mechanism itself**, read while that
///    mechanism is running. The snapshot cannot serve these because it is
///    latched before the step that would populate them — see the row for
///    `CRATONVM_ALLOW_UNKNOWN_TOKENS` for the only current instance, which
///    names the two lines that establish the ordering.
///
/// A flag read by anything under a crate's `src/` does **not** qualify. If one
/// is added here, the reason string has to say why the snapshot cannot serve
/// that call site — "it was easier" is not a reason.
const ALLOWED: &[(&str, &str)] = &[
    (
        "CRATONVM_BUILD_COMPILER",
        "build ABI: an `env!` the vm-cli build script (`vm-cli/build.rs`) sets at COMPILE time for \n         `sun.rt.internalVersion`'s `built ... with <compiler>` tail; never read from the run-time \n         environment, so no token can reach it.",
    ),
    (
        "CRATONVM_BUILD_TIME",
        "build ABI: the `env!` twin of `CRATONVM_BUILD_COMPILER` (the `built on <time>` tail), set by \n         the same build script at compile time.",
    ),
    (
        "CRATONVM_CLASS_MIRROR_",
        "kind 3: not a read. The prefix of a `concat!` in the unit test \
         `zt8cl4_the_forcing_switch_has_no_reader` (vm/src/vm/vm_object.rs), which \
         asserts that the RETIRED slot-0 overlay key (zt t8/cl4) has no reader left \
         in `config.rs`. The test builds the key from two halves so the full name \
         never appears as a literal; nothing reads it from the environment.",
    ),
    (
        "CRATONVM_FDLIMIT_TEST_CHILD",
        "kind 3: a TEST HARNESS switch, set on its own re-exec child by \
         `vm/src/vm/vm_init.rs::open_file_limit_tests` to mark the child \
         process under a lowered file-descriptor limit.",
    ),
    (
        "CRATONVM_ICC_PROFILE_DIR",
        "kind 3: a TEST HARNESS input, read by the unit tests of `native-awt/src/java2d/icc.rs` to find the JDK's extracted `cmm/profiles` (skipped when unset). It names a DIRECTORY, not a VM behaviour, and never reaches a running VM.",
    ),
    (
        "CRATONVM_JDK25_HOME",
        "kind 3: a TEST HARNESS input, read by the `#[ignore]`d `native-builtins/tests/r15_shadow_thin_wrapper_census.rs` to find a JDK 25 runtime image whose `lib/modules` it reads. It names a DIRECTORY, not a VM behaviour, and never reaches a running VM, so there is no `VmFlags` snapshot to serve it and no `CRATONVM_<GROUP>=token` spelling an operator could set that would mean anything.",
    ),
    (
        "CRATONVM_ZGC_MARKEND_LOG",
        "kind 3: a TEST HARNESS input, read by `gc/tests/zgc_r_conc_window_fit.rs` to point the concurrent-window analysis at a `--verbose:gc` log captured from a previous run. It names a FILE PATH, not a VM behaviour: unset, the test is an ordinary unit test of its own arithmetic and its parser; set, it parses a log this process did not produce. It never reaches a running VM, so there is no `VmFlags` snapshot to serve it and no `CRATONVM_<GROUP>=token` spelling an operator could set that would mean anything.",
    ),
    (
        "CRATONVM_",
        "kind 1: the bare prefix, never a variable. Two test helpers named \
         `with_env` (`vm/src/config.rs`, `libcratonvm/src/lib.rs`) \
         `debug_assert!` that the key they are about to `set_var` does NOT \
         start with it — they are refusing to touch declared flags — and \
         `vm/src/vm/vm_init.rs` uses it to pick which of `std::env::vars()` to \
         report as VM configuration. `exact_literals` matches it because a \
         quote follows the underscore immediately; \
         `the_scanner_only_matches_whole_string_literals` pins that.",
    ),
    (
        "CRATONVM_DBG_FLAGREADS",
        "kind 2: read by the flag machinery ITSELF. `types/src/flags.rs` traces \
         every flag read, so it consults this one with a raw \
         `std::env::var_os` rather than `runtime_var_os` — routing it through \
         the latched snapshot would mean asking the tracer to trace the read \
         that decides whether tracing is on. It therefore cannot be served by \
         `VmFlags`, which is the property every other entry in the inventory \
         asserts, so it is exempt rather than declared.",
    ),
    (
        "CRATONVM_COMPATIBILITY_JDK_ONLY",
        "kind 1: the name of a `libcratonvm` C ABI integer constant, matched \
         here only because a unit test asserts the diagnostic message names it",
    ),
    (
        "CRATONVM_TEST_PIPE_FLOOD",
        "kind 4: a TEST-HARNESS re-entry switch, not a VM knob. \
         `vm/tests/class_loader_unload_regression.rs` re-executes ITS OWN test \
         binary with this set, which is what turns the otherwise-inert \
         `flood_helper` test into the child half of \
         `wait_draining_survives_a_child_that_outruns_the_pipe` -- there is no \
         shell command that floods a pipe on both Linux and Windows, so the \
         child has to be this binary. It is read with a raw `std::env::var_os` \
         before any VM exists and nothing under any `src/` reads it, so a \
         `CRATONVM_<GROUP>=` token for it would name a switch the VM can never \
         consult.",
    ),
    // `CRATONVM_FOO` — the stand-in name in the `flags` module docs — is
    // deliberately *not* here: it only ever appears inside prose, which
    // `is_comment_line` already drops, and a row for it would be dead on
    // arrival under `the_allowlist_has_no_dead_rows`.
    (
        "CRATONVM_DBG_FLAGREADS",
        "kind 3: the one flag that CANNOT be served from the snapshot, because \
         it instruments the snapshot's own reads. `types/src/flags.rs` reads it \
         with `std::env::var_os` directly and says why on the line above: \
         `reading through this module would recurse`. Declaring it would make \
         the flag machinery call itself to decide whether to trace a call to \
         itself.",
    ),
    (
        "CRATONVM_NONEXISTENT_VAR_12345",
        "kind 2: `vm.rs`'s System.getenv coverage needs a name that is \
         guaranteed absent",
    ),
    (
        "CRATONVM_SOMETHING_BRAND_NEW",
        "kind 2: `flag_groups.rs` proves an unknown key falls through `resolve` \
         unchanged, which requires a key no entry claims",
    ),
    (
        "CRATONVM_REAL_RAF",
        "kind 1: a retired gate. Real-RAF is the default and the opt-out is \
         `CRATONVM_SYNTHETIC_RAF`. The surviving mentions are all in COMMENTS, \
         which `is_comment_line` drops, plus the `env_remove` baseline list in \
         `vm/tests/synthetic_diff.rs` (which is why this row exists at all). \
         An earlier version of this note claimed that list was the ONLY \
         mention; it was not — `native-io/src/lib.rs` named the retired \
         variable in the RAF gate's own comment until H8-1 corrected it. \
         Delete this row when the `env_remove` list is trimmed.",
    ),
    (
        "CRATONVM_DIFF_HOTSPOT",
        "kind 3: `vm/tests/jit_interp_differential.rs` — opts the differential \
         harness into running the reference JVM",
    ),
    (
        "CRATONVM_FUZZ_BOOTCP",
        "kind 3: `fuzz/fuzz_targets/fuzz_verifier.rs` — boot classpath for a \
         `cargo fuzz` target, read before any VM exists",
    ),
    (
        "CRATONVM_REGEN_HEADER",
        "kind 3: `libcratonvm/build.rs` — asks the build script to re-run \
         cbindgen. A build script runs in a different process from the VM.",
    ),
    (
        "CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS",
        "kind 3: `vm/tests/interpreter_tests.rs` — opts into the slow \
         interpreter corpus",
    ),
    (
        "CRATONVM_SPRING_BOOT_FATJAR",
        "kind 3: `vm/tests/wave3_spring_boot_fatjar.rs` — path to a fixture jar \
         that is not checked in",
    ),
    (
        "CRATONVM_TEST_CLASSES_DIR",
        "kind 3: set by `vm/build.rs` via `cargo:rustc-env` and read with \
         `option_env!`, so it is a compile-time constant, not a runtime flag",
    ),
    (
        "CRATONVM_REQUIRE_E2E",
        "kind 3: `vm/tests/common/mod.rs` - promotes a SKIPPED end-to-end \
         prerequisite to a failure. Read by the harness while it decides \
         whether to stand a VM up at all, so there is no snapshot to \
         serve it.",
    ),
    (
        "CRATONVM_ALLOW_UNKNOWN_TOKENS",
        "kind 4: `vm-cli/src/main.rs` - makes an unrecognised token \
         non-fatal. It is read from the result of \
         `flag_groups::expand_process_env()`, and `install_flags` latches \
         the snapshot fifteen lines EARLIER - so a token spelling of this \
         knob could not be in the snapshot at the moment it is needed. A \
         scalar is not available either: the surface is pinned at fifteen \
         names by `the_whole_surface_is_fifteen_variables`.",
    ),
    (
        "CRATONVM_NO_MISPLACED_FLAG_WARNING",
        "kind 4: `vm-cli/src/main.rs` - silences the warning that a launcher \
         option was passed AFTER the main class and therefore ignored. \
         `warn_about_misplaced_launcher_flags` is called from `main` about \
         thirty-seven lines BEFORE `install_flags` latches the snapshot, \
         because the warning is about argv and has to be reachable whatever \
         the configuration turns out to be. Reading it through \
         `flags::runtime_var_os` therefore latches the snapshot early and \
         `install_flags` fails with `runtime flags were read before launcher \
         configuration` - every run then exits 1, which is how this row was \
         arrived at rather than by argument. The two lines that establish the \
         ordering are the `warn_about_misplaced_launcher_flags(&early_argv)` \
         call and the `install_flags(runtime_flags)` check below it.",
    ),
    (
        "CRATONVM_TEST_JAVA_HOME",
        "kind 3: `vm/tests/*` — a JDK for the test harness to shell out to, \
         checked ahead of `JAVA_HOME`",
    ),
    (
        "CRATONVM_JIT_BASELINE_FAST",
        "kind 3: baseline compilation toggle introduced in Finding #68 ratchet",
    ),
    (
        "CRATONVM_JIT_BASELINE_NO_SPEC",
        "kind 3: alias for CRATONVM_JIT_BASELINE_FAST (Finding #68)",
    ),
    (
        "CRATONVM_IMAGE_ARENA_TEST_CHILD_FILE",
        "test binary: the Phase 5 capture-arena unit test (`vm-cli/src/image_arena.rs`) re-runs itself as a child and passes the result file through this name; no VM run reads it.",
    ),
    (
        "CRATONVM_P5CP_DUMP",
        "test lever: `classloading/tests/jimage_boot_ingestion.rs` writes its lookup-answer digest when set; read only by that test binary.",
    ),
    (
        "CRATONVM_P5CP_REPS",
        "test lever: the repetition count of `classloading/tests/jimage_boot_ingestion.rs`'s timing loop; read only by that test binary.",
    ),
    (
        "CRATONVM_REGBENCH_OUT",
        "test lever: `native-builtins/tests/registration_pass_bench.rs` writes its registry census here when set; read only by that test binary.",
    ),
    (
        "CRATONVM_REGBENCH_ROUNDS",
        "test lever: warm-pass count of `native-builtins/tests/registration_pass_bench.rs`, differenced across two values to measure one pass; read only by that test binary.",
    ),
    (
        "CRATONVM_LOOKUP_DEFINE_JDK_BODY",
        "unit-test flag in `native-builtins/src/lookup_define/tests.rs`.",
    ),
];

fn declared() -> BTreeSet<&'static str> {
    INVENTORY
        .iter()
        .flat_map(|e| [e.on_key, e.off_key].into_iter().flatten())
        .chain(SCALARS.iter().copied())
        .chain(Group::ALL.iter().map(|g| g.var()))
        .collect()
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("types/ always has a workspace root above it")
        .to_path_buf()
}

fn rust_sources(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if !SKIPPED_DIRS.contains(&name) && !name.starts_with('.') {
                rust_sources(&path, out);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// Whether `line` is entirely a comment, so any name in it is prose.
///
/// The `*` cases cover the continuation lines of a `/* … */` block. They are
/// spelled out rather than written as a bare `starts_with('*')`, which is the
/// obvious version and is **wrong**: this tree is full of
/// `*ON.get_or_init(|| std::env::var("CRATONVM_…"))`, and a leading-`*` rule
/// silently swallows every one of those — a guard that skips exactly the
/// once-cached flag reads it exists to find. That mistake was made and caught
/// while writing this file; `the_scanner_only_matches_whole_string_literals`
/// pins it.
///
/// This deliberately does not track block-comment state across lines: an
/// opening `/*` whose body starts on the same line is rare enough, and the cost
/// of a false positive is a one-row allowlist entry, not a wrong answer.
fn is_comment_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || t == "*" || t.starts_with("* ") || t.starts_with("*/")
}

/// Every exact `"CRATONVM_…"` string literal in `line`, in source order.
///
/// "Exact" means the closing quote follows the name immediately — see the
/// module docs for the three shapes that rules out.
fn exact_literals(line: &str) -> Vec<&str> {
    const OPEN: &str = "\"CRATONVM_";
    let mut found = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find(OPEN) {
        // Step past the opening quote only, so an overlapping second match on
        // the same line is still reachable.
        let after_quote = at + 1;
        let name_start = after_quote;
        let tail = &rest[name_start..];
        let end = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        if tail[end..].starts_with('"') {
            found.push(&tail[..end]);
        }
        rest = &rest[after_quote..];
    }
    found
}

/// Scan the workspace and return `(name, sites)` for every exact literal.
fn scan() -> std::collections::BTreeMap<String, Vec<String>> {
    let root = workspace_root();
    let mut sources = Vec::new();
    rust_sources(&root, &mut sources);
    assert!(
        sources.len() > 100,
        "only found {} Rust sources under {} — the walk is not reaching the \
         workspace, so this guard would pass vacuously",
        sources.len(),
        root.display()
    );

    let this_file = Path::new(file!()).file_name().expect("test file name");
    let mut hits: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for path in &sources {
        // This file names every allowlisted variable as a literal.
        if path.file_name() == Some(this_file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if !text.contains("CRATONVM_") {
            continue;
        }
        let relative = path.strip_prefix(&root).unwrap_or(path);
        let shown = relative.display().to_string().replace('\\', "/");
        for (index, line) in text.lines().enumerate() {
            if is_comment_line(line) {
                continue;
            }
            for name in exact_literals(line) {
                hits.entry(name.to_string())
                    .or_default()
                    .push(format!("{shown}:{}", index + 1));
            }
        }
    }
    hits
}

#[test]
fn every_cratonvm_literal_is_declared_or_explicitly_exempt() {
    let declared = declared();
    let hits = scan();

    let mut offenders: Vec<String> = Vec::new();
    for (name, sites) in &hits {
        if declared.contains(name.as_str()) {
            continue;
        }
        if ALLOWED.iter().any(|(allowed, _)| *allowed == name.as_str()) {
            continue;
        }
        offenders.push(format!("{name}\n      first read at {}", sites[0]));
    }

    assert!(
        offenders.is_empty(),
        "{} CRATONVM_* variable(s) are read by code but declared nowhere.\n\n\
         An undeclared flag is served by a live `getenv` rather than by the \
         latched `VmFlags` snapshot, so `CRATONVM_<GROUP>=token` cannot reach \
         it and `flags::with_thread_overrides` cannot arrange it in a test — \
         which is how a flag-dependent test ends up silently measuring the \
         developer's ambient environment.\n\n\
         Add a token for each to `types/src/flag_groups.rs::INVENTORY` and the \
         name to `types/tests/flag-surface.txt`, reuse an existing token \
         instead of minting a variable, or — only for a harness/ABI name — add \
         a row to `ALLOWED` in this file with the reason.\n\n  {}",
        offenders.len(),
        offenders.join("\n  ")
    );
}

/// The reverse of `every_cratonvm_literal_is_declared_or_explicitly_exempt`: a
/// key that is DECLARED but that no Rust source reads any more.
///
/// A knob whose last reader was deleted or renamed still appears in
/// `docs/CONFIG.md` and in the generated tables, where it reads as a supported
/// lever an operator can set — and setting it does nothing. That is worse than
/// an undeclared flag, which at least still works.
///
/// This existed only in `tools/flag-census/check-surface.sh`, which runs on the
/// Linux CI leg *after* a push has already landed on `dev`. The `pre-push` hook
/// runs this file, so putting the check here is what makes it a gate rather
/// than a report — and it is free: `scan()` has already walked every source for
/// the test above.
///
/// Found by it on the day it was written: `CRATONVM_JIT_IR_GATED_REF_STORE`,
/// left behind when its reader was renamed to `CRATONVM_JIT_IR_REF_STORE` on
/// 2026-09-02. It had been advertised as a working switch ever since.
#[test]
fn every_declared_flag_still_has_a_reader() {
    let sites = scan();
    // A name counts as READ only where it is read — not where it is DECLARED.
    // `scan()` walks every Rust source, and `flag_groups.rs` is one: each
    // `E { .. on_key: Some("CRATONVM_X") .. }` row puts that literal in a Rust
    // file, so without this filter every declared flag looks like it has a
    // reader and this test passes for all possible inputs.
    //
    // That is not hypothetical. Written without it, this test passed with
    // `CRATONVM_JIT_IR_GATED_REF_STORE` — a knob with no reader at all —
    // deliberately re-added to the inventory. A guard that cannot fail is worse
    // than no guard: it reports the property it does not check.
    const DECLARATION_SITES: &[&str] = &["types/src/flag_groups.rs"];
    let read: BTreeSet<&str> = sites
        .iter()
        .filter(|(_, where_read)| {
            where_read
                .iter()
                .any(|site| !DECLARATION_SITES.iter().any(|d| site.starts_with(d)))
        })
        .map(|(k, _)| k.as_str())
        .collect();
    let mut dead: Vec<&str> = declared()
        .into_iter()
        .filter(|name| !read.contains(name))
        // A GROUP variable (`CRATONVM_JIT`, `CRATONVM_GC`, ...) is parsed by the
        // flag machinery from its own table rather than named as a literal at a
        // read site, and the five scalars are read through typed accessors. Both
        // are declared-by-construction, so absence from the literal scan says
        // nothing about them.
        .filter(|name| !Group::ALL.iter().any(|g| g.var() == *name))
        .filter(|name| !SCALARS.contains(name))
        .collect();
    dead.sort_unstable();
    assert!(
        dead.is_empty(),
        "{} declared CRATONVM_* variable(s) are read by no Rust source.

         A knob whose last reader was deleted or renamed still appears in \
         docs/CONFIG.md and in the generated tables, where it reads as a \
         supported lever — and setting it does nothing.

         Remove the row from `types/src/flag_groups.rs::INVENTORY` and the name \
         from `types/tests/flag-surface.txt`, then regenerate both docs:
            python tools/flag-census/render-inventory.py .
            bash   tools/flag-census/render-tokens.sh

         If the reader was RENAMED, declare the new name rather than deleting \
         the knob.

  {}",
        dead.len(),
        dead.join(
            "
  "
        ),
    );
}

#[test]
fn the_allowlist_has_no_dead_rows() {
    let hits = scan();
    let declared = declared();

    let mut stale: Vec<String> = Vec::new();
    for (name, reason) in ALLOWED {
        if declared.contains(name) {
            stale.push(format!(
                "{name}: now declared, so the exemption is dead ({reason})"
            ));
        } else if !hits.contains_key(*name) {
            stale.push(format!("{name}: no read site left ({reason})"));
        }
    }

    assert!(
        stale.is_empty(),
        "the exemption list has {} row(s) that no longer apply. An allowlist \
         nobody prunes is how the last flag surface reached 692 names — delete \
         them:\n  {}",
        stale.len(),
        stale.join("\n  ")
    );
}



/// gen r4w3/obs2 (2026-09-23): the launcher's "set but claimed by no knob"
/// advisory (`flag_groups::unclaimed_env_names`) must not name a variable this
/// file exempts on purpose, so the runtime keeps a copy of `ALLOWED`'s names
/// (`flag_groups::UNDECLARED_READS`, prefix-less — see its docs). This keeps
/// the copy exact in both directions: a row added or pruned here fails until
/// the runtime list follows. The bare `CRATONVM_` prefix row is not a
/// variable and has no counterpart.
#[test]
fn the_runtime_unclaimed_list_is_the_guard_allowlist() {
    use cratonvm_types::flag_groups::UNDECLARED_READS;
    let allowed: BTreeSet<String> = ALLOWED
        .iter()
        .map(|(name, _)| (*name).to_string())
        .filter(|name| name != "CRATONVM_")
        .collect();
    let runtime: BTreeSet<String> = UNDECLARED_READS
        .iter()
        .map(|suffix| format!("CRATONVM_{suffix}"))
        .collect();
    let missing: Vec<_> = allowed.difference(&runtime).collect();
    let extra: Vec<_> = runtime.difference(&allowed).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "flag_groups::UNDECLARED_READS must hold exactly ALLOWED's names (without \
         the CRATONVM_ prefix). Missing there: {missing:?}. Not in ALLOWED: {extra:?}."
    );
}
