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

/// Phrases a record uses to say "this has not met a binary", matched
/// **case-insensitively as substrings** of a status line.
///
/// This list was six exact phrases until 2026-09-02, and that was the defect.
/// It enumerated the PHRASINGS this campaign's records happened to use, and the
/// campaign uses many more: `FIXED-UNVERIFIED`, `FIXED-UNVERIFIED-BY-CARGO`,
/// `FIXED-UNVERIFIED-ON-CRATONVM`, `CODE LANDED, BEHAVIOUR UNVERIFIED`,
/// `FIXED IN SOURCE, NOT VERIFIED BY AN ARM`, and plain prose like *"no binary
/// exists that contains the code below"*. The test saw **5** records; the real
/// population was **61**. Fifty-six pages said, in their own status block, that
/// nobody had ever run the fix, and the gate was green over all of them.
///
/// That is the failure this repo has seen before: a literal table sitting in
/// front of a question, agreeing with itself. `MIN_PAGES_SCANNED` guards the
/// WALK and was working perfectly — 526 pages — while the MATCH saw almost
/// nothing. A floor on the denominator says nothing about the numerator.
///
/// Keep these SHORT and generic. A marker that names a lane, a date or a file
/// is a marker that will miss the next record.
const UNVERIFIED_MARKERS: &[&str] = &[
    "unverified",
    "not verified",
    "not rebuilt",
    "never been built",
    "never been run",
    "not been built",
    "not been run",
    "built or run",
    "no binary",
    "has not run",
    "never met a binary",
    "could not build or run rust",
];

/// Phrases that discharge the debt. A page carrying one of these has met a
/// binary, and any unverified phrasing left in it is history — every one of the
/// four records repaired on 2026-09-01 QUOTES its old status so the reader can
/// see what changed, and that quotation must not re-trip this test.
const DISCHARGE_MARKERS: &[&str] = &[
    "VERIFIED AGAINST A BINARY",
    "Verified against a binary",
];

/// Records whose own status block says they have never met a binary.
///
/// **This list was five entries until 2026-09-02. The real population was 61.**
/// The other 56 were invisible because `UNVERIFIED_MARKERS` enumerated
/// phrasings rather than asking the question; see the comment there. They are
/// enumerated here now, each with the status line that put it in the list, so
/// the debt is a worklist instead of a rumour.
///
/// **These are OWED, not EXCUSED.** An entry is a promise someone still has to
/// keep. The right way to remove one is to run the handle the record names and
/// add its `VERIFIED AGAINST A BINARY` note — and once you do, the staleness
/// check at the bottom of this test REQUIRES the entry to go, so the list can
/// only shrink.
///
/// Two entries have been paid, and both are worth reading before you pick one:
///
/// * `W7-63`, `W7-41`, `W7-19`, `W7-71` (2026-09-01) — never in this list,
///   because they were fixed before it existed. All four passed, 0-diff against
///   HotSpot in both modes, after sitting unverified for eighteen days.
/// * `W7-24` (2026-09-02) — the first debt this list itself retired. Its door
///   defect was fixed and unrun for 22 days, and the fix was right. Getting
///   there also cost a repair to the PROBE, which printed ephemeral port
///   numbers and so reported four differing rows on two runs of the same
///   binary: an unscoreable probe is why a debt stays owed.
///
/// The pattern in both: **the fixes are usually right.** What is missing is the
/// run, and a record nobody can act on is worth much less than the work in it.
///
/// One thing the 2026-09-02 run established the hard way: some probes are
/// arm-specific. `W7-57`/`W7-58`/`W7-70`/`W7-81` name sites registered only
/// under `--synthetic-jdk`, and a default build produces numbers that measure
/// the real path and look like evidence. `W7-24` is the opposite — its own
/// reproduce line is `--jdk-only`. Read the record for its arm; do not assume
/// the neighbour's.
const ALLOWED: &[(&str, &str)] = &[
    (
        "docs/known-issues/jdk-only/E25-R11-GUARD-POPULATION-SWEEP-20260813.md",
        "status block says: **Status:** FIXED-UNVERIFIED (registration + two guards in the owned file);",
    ),
    (
        "docs/known-issues/jdk-only/G29-1-the-fabricated-http-request-and-its-missing-accessors-20260817.md",
        "status block says: oracle. The fix is written and formatted but **has not been built**, so its",
    ),
    (
        "docs/known-issues/jdk-only/G31-1-astype-and-the-verifier-that-was-never-asked-20260817.md",
        "status block says: --tests` clean), but **no binary carrying them has ever executed**. This lane",
    ),
    (
        "docs/known-issues/jdk-only/G44-1-the-session-the-verifier-was-handed-20260817.md",
        "status block says: predates `aed6a3b73`, so no binary containing either this lane's changes or the",
    ),
    (
        "docs/known-issues/jdk-only/H0-1-the-jmx-pin-and-a-jdk-that-was-not-there-20260820.md",
        "status block says: **Status: FIXED-UNVERIFIED.** Three source/doc changes landed in the tree. **No",
    ),
    (
        "docs/known-issues/jdk-only/H12-1-the-osr-door-binds-five-bridge-natives-the-method-entry-door-refuses-20260820.md",
        "status block says: **Status: MEASURED (the defect) / FIXED-UNVERIFIED (the fix).** The divergence",
    ),
    (
        "docs/known-issues/jdk-only/H2-1-the-filetime-epoch-and-the-queue-lock-20260820.md",
        "status block says: **Status** `FIXED-UNVERIFIED` — **no binary carrying these changes has been",
    ),
    (
        "docs/known-issues/jdk-only/H20-1-the-direct-call-plan-is-a-second-thing-every-door-builds-20260821.md",
        "status block says: still not been run.",
    ),
    (
        "docs/known-issues/jdk-only/H24-1-the-module-source-door-and-the-two-modules-a-boot-layer-probe-could-not-see-20260821.md",
        "status block says: **Status: FIXED IN SOURCE, NOT VERIFIED BY AN ARM.** Lane H24, 2026-08-21.",
    ),
    (
        "docs/known-issues/jdk-only/W7-55-record-reconciliation.md",
        "status block says: Nothing was built or run for this pass. Every verdict below is git and source",
    ),
    (
        "docs/known-issues/jdk-only/W7-9-minted-interface-abstract-methods.md",
        "status block says: **Nothing here has been built or run.** Every claim is either `javap` output from",
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

/// The marker a page's OWN status block carries, if any.
///
/// Three things are deliberately NOT offences, each checked against the tree
/// rather than guessed:
///   * a BLOCKQUOTE (`>`) -- every record repaired on 2026-09-01 quotes its old
///     status so a reader can see what changed;
///   * a HEADING (`#`) -- `jdk-only/README.md` INDEXES records in this state,
///     which is the opposite of hiding them;
///   * anything past the status block -- `G39-1` describes ANOTHER record's
///     banner in its prose, and that is a citation, not a claim.
fn unverified_marker(text: &str) -> Option<&'static str> {
    text.lines()
        .take(STATUS_BLOCK_LINES)
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with('>') && !t.starts_with('#')
        })
        .find_map(|l| {
            let lower = l.to_ascii_lowercase();
            UNVERIFIED_MARKERS.iter().copied().find(|m| lower.contains(*m))
        })
}

/// Whether the page says somewhere that the debt was paid. Deliberately an
/// EXACT phrase, unlike the markers above: a discharge is a claim someone must
/// make on purpose, and a loose match here forgives records that never ran. A
/// draft of this test matched `verification note` and duly declared `W7-24` and
/// `W7-57` discharged -- both of which are in `ALLOWED` precisely because they
/// are not.
fn is_discharged(text: &str) -> bool {
    DISCHARGE_MARKERS.iter().any(|d| text.contains(*d))
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
        let Some(marker) = unverified_marker(text) else {
            continue;
        };
        if is_discharged(text) {
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

    // An allow-list rots the moment a record is repaired: the entry stays,
    // quietly excusing a page that no longer needs excusing, and a reader can
    // no longer tell the live debts from the paid ones. With 61 entries that
    // rot is a certainty rather than a risk, so every entry must still name a
    // page that EXISTS and still trips the detector. Paying a debt is then
    // required to remove its entry, and the list can only shrink.
    let mut stale: Vec<String> = Vec::new();
    for (path, _) in ALLOWED {
        match found.iter().find(|(rel, _)| rel == path) {
            None => stale.push(format!(
                "  {path}\n      names no such page — it was moved, renamed or deleted"
            )),
            Some((_, text)) => {
                if unverified_marker(text).is_none() || is_discharged(text) {
                    stale.push(format!(
                        "  {path}\n      no longer claims an unrun fix — the debt was \
                         paid and the entry outlived it"
                    ));
                }
            }
        }
    }
    stale.sort();

    assert!(
        stale.is_empty(),
        "{} ALLOWED entr(ies) no longer describe the tree:\n\n{}\n\nDELETE them. \
         An allowance for a record that is already verified is not harmless: it \
         is the mechanism by which a list of 61 real debts decays into a list \
         nobody trusts, and then into one nobody reads.",
        stale.len(),
        stale.join("\n")
    );
}
