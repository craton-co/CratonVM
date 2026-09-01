// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A record must not claim a fix it has never run.
//!
//! # The seam this closes
//!
//! Some lanes can write Rust and cannot execute it — no toolchain, no build
//! host, or simply no time on the shared box. Those lanes land real fixes and
//! say so honestly: `W7-63-jca-advertise-vs-serve.md` shipped 2026-08-12 with
//! *"This lane could not build or run Rust ... nothing here has been observed on
//! a CratonVM binary. The verification command is in §9."*
//!
//! Nobody ran it. It sat for **eighteen days**, and three sibling records sat
//! with it — `W7-41`, `W7-19`, `W7-71`, all "fixed in source" since 2026-08-11.
//! All four were finally run on 2026-09-01 and all four passed, 0-diff against
//! HotSpot in both modes. Eighteen days of correct fixes nobody could rely on,
//! because the honest disclaimer was also invisible.
//!
//! The disclaimer is right and should stay. What was missing is anything that
//! COUNTS them, so the debt shows up as a red instead of as prose in one file
//! among hundreds.
//!
//! # What this asserts
//!
//! Every `docs/known-issues/**/*.md` carrying an unverified-status marker must
//! also carry a discharge marker. A record may be landed unverified — that is
//! the point of the disclaimer — but it then owes a run, and this test is the
//! reminder that does not depend on anyone remembering.
//!
//! # How to make it green
//!
//! **Run the thing.** Every one of these records names its own handle: a
//! regression-suite vector, a probe, a `cargo test` target. Run it on a release
//! binary against the JDK oracle, then add to the record:
//!
//! ```text
//! > **VERIFIED AGAINST A BINARY <date>.** <vector> <result>, N differing lines.
//! ```
//!
//! Two cautions learned from doing exactly that for the four above:
//!
//! * **Check the binary's timestamp before believing a run.** A fat-LTO link
//!   killed by the OOM reaper leaves the previous binary in place and `cargo`'s
//!   exit code does not stop the script that follows it.
//! * **A predicted check COUNT is not an assertion.** All four records
//!   predicted one and all four were wrong — 80 vs 153, 47 vs 57, 37 steps vs
//!   40 — because other lanes grew the shared vectors. Verify the ASSERTIONS;
//!   note the count drift rather than diagnosing a defect from it.
//!
//! If a record genuinely cannot be verified, add it to `ALLOWED` with the
//! reason. An allowance nobody can justify is worse than a red.

use std::path::{Path, PathBuf};

/// Phrases a record uses to say "this has not met a binary".
const UNVERIFIED_MARKERS: &[&str] = &[
    "NOT yet verified against a binary",
    "UNVERIFIED against a VM",
    "NOT REBUILT",
    "not been observed on a CratonVM binary",
    "could not build or run Rust",
    "unverified against a binary",
];

/// Phrases that discharge the debt. A page carrying one of these has met a
/// binary, and any unverified phrasing left in it is history — every one of the
/// four records repaired on 2026-09-01 QUOTES its old status so the reader can
/// see what changed, and that quotation must not re-trip this test.
const DISCHARGE_MARKERS: &[&str] = &[
    "VERIFIED AGAINST A BINARY",
    "Verified against a binary",
];

/// The debt as it stood when this test was written: records whose own status
/// block says they have never met a binary.
///
/// **These are OWED, not EXCUSED.** An entry here is a promise someone still has
/// to keep, and the right way to remove one is to run the handle the record
/// names and add its `VERIFIED AGAINST A BINARY` note — not to leave it sitting
/// because the list already tolerates it.
///
/// The four this test was born from — `W7-63`, `W7-41`, `W7-19`, `W7-71` — are
/// deliberately absent: they were verified on 2026-09-01 and all four passed,
/// 0-diff against HotSpot in both modes. That is the outcome this list exists to
/// make ordinary, and it is the evidence that working the list down is cheap.
const ALLOWED: &[(&str, &str)] = &[
    (
        "docs/known-issues/jdk-only/W7-24-httpserverloop-and-strict-fallbacks.md",
        "source landed 2026-08-12, never run; names RArraysMismatch among its handles",
    ),
    (
        "docs/known-issues/jdk-only/W7-57-close-flush-swallow-sweep.md",
        "source landed 2026-08-12, never run; names RLClassPath",
    ),
    (
        "docs/known-issues/jdk-only/W7-58-bytebuffer-direct-arm.md",
        "source landed 2026-08-12, never run; names RJdkNio and RDirectBufferElem",
    ),
    (
        "docs/known-issues/jdk-only/W7-70-printstream-close-noop.md",
        "source landed 2026-08-12, never run; handle not named in the record",
    ),
    (
        "docs/known-issues/jdk-only/W7-81-write-route-three-way.md",
        "source landed 2026-08-12, never run; handle not named in the record",
    ),
];

/// The walk must see at least this many pages, or it is broken rather than the
/// tree being clean. Without this a mistyped path passes silently — the
/// "confident, vacuous zero" this repo's other ratchets guard against.
const MIN_PAGES_SCANNED: usize = 200;

/// How far into a page its status block reaches. A record states what it is
/// before it states anything else; text further down is prose about OTHER
/// records, and treating that as a claim made this test fire on a page whose
/// only sin was citing a sibling accurately.
const STATUS_BLOCK_LINES: usize = 12;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("types/ always has a workspace root above it")
        .to_path_buf()
}

fn pages(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if !name.starts_with('.') {
                pages(&path, root, out);
            }
        } else if name.ends_with(".md") {
            if let (Ok(rel), Ok(text)) = (path.strip_prefix(root), std::fs::read_to_string(&path)) {
                out.push((rel.to_string_lossy().replace('\\', "/"), text));
            }
        }
    }
}

#[test]
fn a_record_does_not_claim_a_fix_it_never_ran() {
    let root = workspace_root();
    let mut found = Vec::new();
    pages(&root.join("docs").join("known-issues"), &root, &mut found);

    assert!(
        found.len() >= MIN_PAGES_SCANNED,
        "only {} known-issues pages scanned (floor {MIN_PAGES_SCANNED}). The walk \
         broke rather than the tree shrinking — repair the scan before trusting \
         a green here.",
        found.len()
    );

    let mut offenders: Vec<String> = Vec::new();
    for (rel, text) in &found {
        if ALLOWED.iter().any(|(p, _)| p == rel) {
            continue;
        }
        // Only a page's OWN status claim counts, and it is always at the top.
        // Three things are deliberately NOT offences, each checked against the
        // tree rather than guessed:
        //   * a BLOCKQUOTE (`>`) -- every record repaired on 2026-09-01 quotes
        //     its old status so a reader can see what changed;
        //   * a HEADING (`#`) -- `jdk-only/README.md` INDEXES records in this
        //     state, which is the opposite of hiding them;
        //   * anything past the status block -- `G39-1` describes ANOTHER
        //     record's banner in its prose, and that is a citation, not a claim.
        let Some(marker) = text
            .lines()
            .take(STATUS_BLOCK_LINES)
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with('>') && !t.starts_with('#')
            })
            .find_map(|l| UNVERIFIED_MARKERS.iter().find(|m| l.contains(**m)))
        else {
            continue;
        };
        if DISCHARGE_MARKERS.iter().any(|d| text.contains(*d)) {
            continue;
        }
        offenders.push(format!("  {rel}\n      says: {marker}"));
    }
    offenders.sort();

    assert!(
        offenders.is_empty(),
        "{} record(s) claim a fix that has never been run on a binary, and carry \
         no verification note:\n\n{}\n\nRun the handle each one names — a \
         regression-suite vector, a probe, a cargo target — on a release binary \
         against the JDK oracle, then add a `VERIFIED AGAINST A BINARY <date>` \
         note with the result. Check the binary's TIMESTAMP first: an \
         OOM-killed fat-LTO link leaves the previous one in place and cargo's \
         exit code will not stop you. If a record truly cannot be verified, add \
         it to ALLOWED with the reason.\n\nThis test exists because four such \
         records sat unverified for eighteen days, and all four passed the \
         moment anyone ran them.",
        offenders.len(),
        offenders.join("\n")
    );
}
