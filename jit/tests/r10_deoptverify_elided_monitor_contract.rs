// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 10, lane `deoptverify`: the standing contract behind
//! `Compiler::has_elided_monitor`, pinned so it cannot be broken silently.
//!
//! `docs/internal/fixed-bugs/r10-earelock-has-elided-monitor-is-never-set-FIXED-20260922.md`
//! found that the field is assigned `false` exactly once and `true` never, so
//! both `can_deopt_resume` and `can_osr_exit` reduce to their first conjunct.
//! The page's own conclusion is that this is **currently correct**, and that is
//! why this file is a ratchet rather than a bug report. Restated, because the
//! distinction is the whole point:
//!
//! * Phase B elided monitors by a BLANKET rule ("this method scalar-replaced
//!   something, so drop its monitor ops"), which could elide a lock on an
//!   unrelated, genuinely escaping receiver and left no trace in the frame. A
//!   method that did that had to be kept off the resume path, and
//!   `has_elided_monitor` is the flag that kept it there.
//! * Phase C replaced the blanket rule with a per-PC proof
//!   (`Compiler::sr_monitor_scalar_ops`) and RECORDS what it elides
//!   (`sr_monitor_at` → a `MonitorInfo { relock: true }` that
//!   `build_frame_state_at` publishes and the resume replays). A recorded
//!   elision must NOT set the flag, or Phase C would exclude the very methods it
//!   exists to keep resumable.
//!
//! So the flag's condition is genuinely never met, and the danger is not the
//! `false` — it is that **a flag which is false forever is indistinguishable
//! from a flag whose condition never happens**. The day someone adds an elision
//! that leaves no `MonitorInfo` — an `ACC_SYNCHRONIZED` fast path, a lock
//! coarsening pass, a receiver proved thread-local by some other analysis — they
//! will read the surrounding prose, believe the guard is live, and not write the
//! assignment. `can_deopt_resume` stays true and the resume skips a re-lock: a
//! monitor left held, and a hang in whatever thread asks for it next, arbitrarily
//! far from the deopt that caused it.
//!
//! The contract these tests encode, in one sentence:
//!
//! > **Any lock elision in this backend that does not record a `MonitorInfo`
//! > must also set `has_elided_monitor`.**
//!
//! Both tests are TEXTUAL scans of `jit/src`. They are deliberately allowed to
//! fail on the legitimate edit — adding the `= true` write is *supposed* to trip
//! the first one — and each failure message says what to do about it. That is the
//! same shape as `process_global_statics_ratchet.rs`: the number is not sacred,
//! the argument for changing it is.
//!
//! WHAT THEY CANNOT SEE. A grep proves "no textual occurrence", never "no
//! behaviour". An elision expressed without mentioning `sr_monitor_scalar_ops`
//! (a new predicate, a table lookup, a flag threaded from elsewhere) is invisible
//! to test 2, which is why test 2 is phrased as "the one elision decision is
//! still the recorded one" rather than "there is no other elision".
//!
//! Written by reading; **read, not executed** — this lane was not permitted to
//! build or run anything.

use std::path::{Path, PathBuf};

/// Every `.rs` file under `jit/src`, recursively.
///
/// Panics on an unreadable directory instead of returning what it has: a scan
/// that silently finds no files passes vacuously, which is the one failure mode
/// a ratchet must not have.
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => panic!("cannot read {}: {e}", dir.display()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn jit_src_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    files.sort();
    assert!(
        !files.is_empty(),
        "found no Rust sources under {}; every scan below would pass vacuously",
        root.display()
    );
    files
}

/// Is `line` code rather than prose? Comment lines are skipped by every scan
/// here, because the token under test appears in a dozen doc comments and those
/// are the thing the page complains ABOUT, not the thing being counted.
fn is_code(line: &str) -> bool {
    let t = line.trim_start();
    !(t.is_empty() || t.starts_with("//") || t.starts_with("*"))
}

/// Does this code line bind `name` — `name:` or `name =` — as opposed to merely
/// reading it?
///
/// The distinction is what the page's confirmation grep rests on. Declaration
/// (`name: bool,`) and initialisation (`name: false,`) both bind; a read
/// (`!compiler.name`) does not, because the character after the identifier is
/// not `:` or `=`.
///
/// `==` is excluded: a comparison reads.
fn binds(line: &str, name: &str) -> bool {
    let mut rest = line;
    while let Some(i) = rest.find(name) {
        let after = &rest[i + name.len()..];
        let trimmed = after.trim_start();
        if trimmed.starts_with(':') || (trimmed.starts_with('=') && !trimmed.starts_with("==")) {
            return true;
        }
        rest = after;
    }
    false
}

/// TEST 1 — the flag's whole lifetime is one declaration and one `false`.
///
/// This is the page's confirmation grep, executable:
///
/// ```text
/// rg -n "has_elided_monitor" --type rust .
/// ```
///
/// with the rule that the only binding occurrences are the field declaration and
/// the constructor's `false`.
///
/// IF THIS TEST FAILS BECAUSE YOU ADDED `has_elided_monitor = true`: good — that
/// is the edit the contract asks for, and the right response is to update the
/// expectation below and retire the "currently unreachable by construction"
/// sentences in `x64.rs`, `x64/driver.rs`, `x64/escape_analysis.rs` and
/// `lib.rs`, which will have become false.
///
/// IF IT FAILS BECAUSE THE FIELD IS GONE: the page's option 2 (retire it) was
/// taken. Delete this file and move the contract sentence onto the monitor arm
/// in `x64/op_object.rs`, as that option requires.
#[test]
fn has_elided_monitor_is_declared_and_initialised_and_never_assigned_true() {
    const FLAG: &str = "has_elided_monitor";

    let mut bindings: Vec<String> = Vec::new();
    for file in jit_src_files() {
        let src = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(e) => panic!("cannot read {}: {e}", file.display()),
        };
        for (i, line) in src.lines().enumerate() {
            if !is_code(line) || !line.contains(FLAG) {
                continue;
            }
            if binds(line, FLAG) {
                bindings.push(format!("{}:{}: {}", file.display(), i + 1, line.trim()));
            }
        }
    }

    // Two, and exactly these two: the `bool` field declaration, and the
    // constructor's `false`. Anything else is a third binding site.
    assert_eq!(
        bindings.len(),
        2,
        "expected exactly the declaration and the `false` initialiser of `{FLAG}`; found \
         {bindings:#?}. See the module doc: a third binding site is either the legitimate \
         `= true` (update this test and the four doc comments) or an accidental shadow."
    );
    assert!(
        bindings.iter().any(|b| b.contains("bool")),
        "one of the two `{FLAG}` bindings must be the field declaration; found {bindings:#?}"
    );
    assert!(
        bindings.iter().any(|b| b.contains("false")),
        "one of the two `{FLAG}` bindings must be the `false` initialiser; found {bindings:#?}"
    );
    assert!(
        !bindings.iter().any(|b| b.contains("true")),
        "`{FLAG}` is assigned `true` somewhere: {bindings:#?}. That is allowed — it is what the \
         contract asks of an untraceable elision — but it makes the \"currently unreachable by \
         construction\" prose in x64.rs / x64/driver.rs / x64/escape_analysis.rs / lib.rs wrong, \
         so update those and this expectation together."
    );
}

/// TEST 2 — the one lock elision this backend performs is still the RECORDED
/// one.
///
/// `sr_monitor_scalar_ops` is the per-PC proof set. It is *consulted* in exactly
/// one place — the `0xC2 | 0xC3` arm of `x64/op_object.rs`, which returns early
/// and drops the receiver without emitting `jit_monitor_enter`/`jit_monitor_exit`
/// — and that is the only code in the backend that removes a lock. The elision
/// is traceable only because a second, independent piece of plumbing exists:
/// `sr_monitor_at`, read by `x64/deopt_stubs.rs::build_frame_state_at` to build
/// the `MonitorInfo` the resume replays.
///
/// So the two halves are pinned together. Removing the recording while keeping
/// the elision converts today's traceable elision into exactly the untraceable
/// kind `has_elided_monitor` was built for — and with the flag never set, nothing
/// else in the tree would object.
///
/// CONTRACT THE ASSERTIONS REST ON, stated because the numbers are the claim:
/// these are counts of CODE lines (comments excluded by `is_code`) containing the
/// token, across `jit/src`. At the time of writing, `sr_monitor_scalar_ops`
/// appears on code lines in `x64.rs` (declaration, constructor),
/// `x64/driver.rs` (the assignment from `sr_plan`) and `x64/op_object.rs` (the
/// one consultation); `sr_monitor_at` in `x64.rs` (declaration, constructor),
/// `x64/driver.rs` (assignment) and `x64/deopt_stubs.rs` (the one read). The
/// assertions below are about the CONSUMER files only, because those are the two
/// facts that carry the contract; the declaration and plumbing sites are free to
/// move.
#[test]
fn the_single_lock_elision_still_records_its_monitor() {
    const ELIDE: &str = "sr_monitor_scalar_ops";
    const RECORD: &str = "sr_monitor_at";

    let files = jit_src_files();
    let mut elision_sites: Vec<String> = Vec::new();
    let mut recording_reads: Vec<String> = Vec::new();

    for file in &files {
        let name = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let src = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => panic!("cannot read {}: {e}", file.display()),
        };
        for (i, line) in src.lines().enumerate() {
            if !is_code(line) {
                continue;
            }
            let at = format!("{}:{}: {}", file.display(), i + 1, line.trim());
            // A CONSULTATION of the proof set, i.e. a MEMBERSHIP TEST — that
            // is what the prose above has always said, and `.contains(` is now
            // what the code checks, because "mentions the set and does not bind
            // it" turned out to catch something that decides nothing.
            //
            // The thing it caught: `x64/driver.rs`'s `monitor_elided_pcs=`
            // trace column iterates the set to PRINT it. Reporting a decision
            // is not making one, and a diagnostic that cannot be added without
            // tripping the ratchet is a ratchet that will be deleted rather
            // than satisfied. Iterating for a report is admitted; asking
            // whether a pc is in the set is not, and that is still exactly one
            // place.
            //
            // `binds` still runs, so the declaration and the `sr_plan`
            // assignment stay excluded whatever they do on the right-hand side.
            if line.contains(ELIDE) && !binds(line, ELIDE) && line.contains(".contains(") {
                elision_sites.push(at.clone());
            }
            // The read that turns the elision into published metadata lives in
            // the deopt snapshot builder; the declaration/assignment sites bind.
            if name == "deopt_stubs.rs" && line.contains(RECORD) && !binds(line, RECORD) {
                recording_reads.push(at);
            }
        }
    }

    assert_eq!(
        elision_sites.len(),
        1,
        "expected exactly ONE consultation of `{ELIDE}` — the monitor arm in \
         jit/src/x64/op_object.rs, which is the only code in this backend that removes a lock. \
         Found {elision_sites:#?}. A second elision decision must either record a `MonitorInfo` \
         (like the first one) or set `has_elided_monitor`; see the module doc."
    );
    assert!(
        elision_sites[0].contains("op_object.rs"),
        "the one `{ELIDE}` consultation must be the monitor lowering in op_object.rs; found {}",
        elision_sites[0]
    );
    assert_eq!(
        recording_reads.len(),
        1,
        "expected exactly ONE read of `{RECORD}` in the deopt snapshot builder — the monitor lane \
         of `build_frame_state_at`, which is what makes the elision above traceable. Found \
         {recording_reads:#?}. If the read is gone, the elision has become untraceable and \
         `has_elided_monitor` must now be set at the elision site."
    );
}
