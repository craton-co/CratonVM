// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Hand-maintained numbers in the top-level documents have to still be true.
//!
//! # The failure this file exists to catch
//!
//! On 2026-09-01 five separate claims in this tree were re-measured and every
//! one of them was wrong, in the same direction and for the same reason: the
//! *generated* artefacts were correct and the *hand-written* numbers sitting
//! beside them were not.
//!
//! | claim | where | reality when measured |
//! |---|---|---|
//! | ZGC is "behind the default-off `zgc` feature, so a stock build does not have it" | `ARCHITECTURE.md`, `CONTRIBUTING.md`, `ROADMAP.md`, `gc/README.md` | default since 2026-08-10 (`gc/Cargo.toml` has `default = ["zgc"]`) |
//! | ZGC has "no colored pointers, load barriers, concurrency, or compaction" | `gc/README.md` | `gc/src/zgc/vaddr.rs`, `barrier.rs`, `zgc_concurrent.rs`, `relocate.rs` all exist |
//! | "~1,350,000 Rust LoC" | `ARCHITECTURE.md` | 2,005,889 by the document's own reproduction command |
//! | the per-crate LoC table | `ARCHITECTURE.md` | every row low by 30-140% |
//! | "692 distinct identifiers / 658 literals" | `docs/config/flag-inventory.md` | 1,056 / 993 |
//!
//! `flag_docs_generated.rs` already guards the two *generated* flag documents,
//! and the rows it guards are exactly the rows that stayed correct. The rows
//! that drifted were the summary lines sitting immediately above them, which
//! nothing checked — including, in `flag-inventory.md`, two counts that went so
//! stale the declared total appeared to *exceed* the number of flags in the
//! source, which is not a possible state of the world. Nobody noticed for
//! twenty-six days, because a number in prose looks the same whether it is
//! measured or remembered.
//!
//! Each test below re-derives its claim from the code and fails with the true
//! value and the command that produces it, so the fix is an edit rather than an
//! investigation.
//!
//! # Direction
//!
//! Every test here reads the **code** as the authority and the **prose** as the
//! claim, never the other way round. None of them will ever ask you to change a
//! `Cargo.toml`, a source file, or a crate layout to satisfy a document.
//!
//! # What is deliberately NOT checked
//!
//! * **The dated parenthetical.** `ARCHITECTURE.md` writes its total as
//!   "roughly 2,010,000 lines (2,005,889 on 2026-09-01)". Only the *undated*
//!   figures are asserted. The parenthesised one carries its own timestamp, and
//!   a number that says when it was taken is a measurement, not a live claim —
//!   forcing an edit to it on every commit would train people to update the
//!   date without re-running the command, which is how the 1,350,000 survived.
//! * **Prose about ZGC's capabilities.** [`no_published_document_calls_zgc_a_default_off_feature`]
//!   checks one phrase, not the whole claim space. "No load barriers" is a
//!   statement about `gc/src/zgc/barrier.rs` whose refutation is a human
//!   reading, and a guard that guessed at it would either miss the next
//!   rewording or fail on an accurate one.
//! * **`docs/internal`.** Those pages are history and legitimately record what
//!   was once true; `gc-crate-audit.md` still says "default-off `zgc`" and is
//!   right to. The walk in [`published_markdown`] never descends into it.
//! * **Row 3 onwards of "Where the surface stands".** The declared count is
//!   already pinned by `flag_docs_generated.rs` against `INVENTORY` itself,
//!   which is a stronger check than re-deriving it here would be.
//!
//! # Why tolerances, and why exact counts elsewhere
//!
//! The LoC figures are rounded, documented as rounded, and exist to tell a
//! newcomer where the mass of the codebase is. Pinning them exactly would put a
//! red test in front of every commit that adds a line, and the predictable
//! response to that is to delete the test. They are checked loosely enough to
//! ignore ordinary growth and tightly enough that the 49% drift that actually
//! happened could not have reached a reviewer. The two flag counts are not
//! estimates — they are the output of a `sort -u | wc -l` — so they are
//! asserted exactly.
//!
//! # A missing marker is a failure, not a skip
//!
//! If a document no longer contains the heading, table header or recipe a test
//! keys off, that test fails saying so. A guard that quietly stops finding its
//! subject is the same disease as an unchecked number: it reports success while
//! checking nothing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Directory names never descended into: build output, VCS metadata, and
/// vendored third-party sources. `ARCHITECTURE.md`'s own reproduction command
/// excludes `*/target/*` and `*/vendor/*` for the same reason.
const SKIPPED_DIRS: &[&str] = &["target", ".git", "vendor", "node_modules"];

/// Re-derivation recipe for row 1 of "Where the surface stands", quoted back to
/// the reader on failure so the fix does not require finding this file.
///
/// It has to MIRROR [`collect_rust`] — same prunes, same start — or the reader
/// who follows the failure message writes a number this test then rejects. The
/// previous form did not, and did not merely disagree: it printed **0**.
///
/// ```text
/// grep -rhoE 'CRATONVM_[A-Z0-9_]+' --include='*.rs' --exclude-dir=target \
///   --exclude-dir=vendor --exclude-dir=node_modules --exclude-dir='.*' .
/// ```
///
/// `--exclude-dir` globs are matched without `FNM_PATHNAME`, so `*` crosses
/// `/`: against the traversal path `./types` the glob `.*` matches `.` then
/// `/types`, and every directory under the starting `.` is pruned. Measured on
/// GNU grep 3.11 — 0 from `.`, 1,390 from an absolute path. A recipe that
/// answers 0 for the whole repository is the kind of wrong that looks like a
/// finding, and a reader who pastes 0 into the table makes this test fail
/// against a number the recipe itself produced.
///
/// `find -mindepth 1` is used instead of more `--exclude-dir` because the same
/// trap bites `-name '.*'`: without `-mindepth 1` it matches the starting `.`
/// and prunes the entire tree.
const ROW1_RECIPE: &str = concat!(
    r#"find . -mindepth 1 \( -name target -o -name vendor -o -name node_modules "#,
    r#"-o -name '.*' \) -prune -o -name '*.rs' -print0 "#,
    r#"| xargs -0 grep -hoE 'CRATONVM_[A-Z0-9_]+' | sort -u | wc -l"#,
);

/// Re-derivation recipe for row 2. Note it scans `<member>/src` only, which is
/// why the two rows are different numbers rather than the same one twice.
const ROW2_RECIPE: &str = r#"for d in $(sed -n 's/^members = \[//p' Cargo.toml | tr -d '"[],'); do [ -d "$d/src" ] && grep -rhoE '"CRATONVM_[A-Z0-9_]+"' "$d/src"; done | tr -d '"' | sort -u | wc -l"#;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("types/ always has a workspace root above it")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {} ({e}) — failing rather than passing without having \
             checked anything",
            path.display()
        )
    })
}

/// `2007503` -> `2,007,503`, so a failure message is comparable to the document
/// it is about without the reader counting digits.
fn commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A markdown cell holding a number: `741,000`, `**987**`, ` 1,056 `. `None`
/// when the cell holds anything else — which is how the `remaining 9` /
/// `< 13,000 each` cell of the LoC table is skipped rather than misparsed.
fn parse_count(cell: &str) -> Option<u64> {
    let mut digits = String::new();
    for c in cell.chars() {
        match c {
            '0'..='9' => digits.push(c),
            ',' | ' ' | '*' | '`' => {}
            _ => return None,
        }
    }
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// The digit-and-comma run immediately after `prefix`.
///
/// The prefixes used below are all long enough to occur exactly once in their
/// document; a short one would silently bind to the wrong sentence.
fn number_after(doc: &str, prefix: &str) -> Option<u64> {
    let at = doc.find(prefix)? + prefix.len();
    let bytes = doc.as_bytes();
    let mut end = at;
    while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b',') {
        end += 1;
    }
    parse_count(&doc[at..end])
}

/// The digit-and-comma run immediately before `suffix`.
fn number_before(doc: &str, suffix: &str) -> Option<u64> {
    let at = doc.find(suffix)?;
    let bytes = doc.as_bytes();
    let mut start = at;
    while start > 0 && (bytes[start - 1].is_ascii_digit() || bytes[start - 1] == b',') {
        start -= 1;
    }
    parse_count(&doc[start..at])
}

/// Deviation of `actual` from `claimed`, in parts per million.
///
/// Integer throughout: the ordering of the failure table must not depend on
/// float formatting, and every quantity here is a line count.
fn deviation_ppm(claimed: u64, actual: u64) -> u64 {
    if claimed == 0 {
        return u64::MAX;
    }
    claimed.abs_diff(actual).saturating_mul(1_000_000) / claimed
}

/// `28_333` -> `2.83%`.
fn percent(ppm: u64) -> String {
    format!("{}.{:02}%", ppm / 10_000, (ppm % 10_000) / 100)
}

/// The `[workspace] members` list from the root `Cargo.toml`, in declaration
/// order. This is the same list `ARCHITECTURE.md`'s recipe means by "the 22
/// member dirs", and reading it here is what stops the count in that sentence
/// from being a third place the number can drift.
fn workspace_members() -> Vec<String> {
    const OPEN: &str = "members = [";
    let manifest = read("Cargo.toml");
    let at = manifest.find(OPEN).unwrap_or_else(|| {
        panic!(
            "the root Cargo.toml has no `{OPEN}` — this test cannot tell which \
             directories ARCHITECTURE.md's recipe covers and refuses to pass \
             without knowing"
        )
    });
    let rest = &manifest[at + OPEN.len()..];
    let end = rest
        .find(']')
        .expect("the [workspace] members list is closed");
    rest[..end]
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Every `*.rs` under `dir`, skipping [`SKIPPED_DIRS`], dotted directories and
/// `docs/internal` (relative to `root`).
fn collect_rust(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if SKIPPED_DIRS.contains(&name) || name.starts_with('.') {
                continue;
            }
            if path == root.join("docs").join("internal") {
                continue;
            }
            collect_rust(&path, root, out);
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// Raw line count of every `*.rs` under `dir`, i.e. what
/// `find … -print0 | xargs -0 cat | wc -l` reports.
///
/// `wc -l` counts newline bytes, and the newline count of a concatenation is
/// the sum of the parts' newline counts, so summing per file is exact rather
/// than approximate.
fn count_rs_lines(dir: &Path) -> u64 {
    let mut files = Vec::new();
    collect_rust(dir, dir, &mut files);
    let mut total = 0u64;
    for path in &files {
        if let Ok(bytes) = std::fs::read(path) {
            total += bytes.iter().filter(|b| **b == b'\n').count() as u64;
        }
    }
    total
}

fn is_flag_byte(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'
}

/// The identifier prefix, spelled in two halves so this file does not itself
/// add a name to the set it counts. (Writing it whole here would make the test
/// change the number it asserts, which is a very confusing way to fail.)
fn flag_prefix() -> String {
    format!("{}{}", "CRATONVM", "_")
}

/// Hand-rolled equivalent of `grep -ohE 'CRATONVM_[A-Z0-9_]+'`.
///
/// `cratonvm-types` has no regex dependency and this pattern does not justify
/// adding one: it is a fixed prefix followed by a maximal run of one character
/// class, which is four lines of `find`. Matches are non-overlapping and the
/// cursor advances past each one, exactly as `grep -o` does.
fn scan_identifiers(text: &str, prefix: &str, out: &mut BTreeSet<String>) {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while let Some(off) = text[i..].find(prefix) {
        let start = i + off;
        let body = start + prefix.len();
        let mut end = body;
        while end < bytes.len() && is_flag_byte(bytes[end]) {
            end += 1;
        }
        if end > body {
            out.insert(text[start..end].to_string());
            i = end;
        } else {
            i = body;
        }
    }
}

/// Hand-rolled equivalent of `grep -ohE '"CRATONVM_[A-Z0-9_]+"' | tr -d '"'`.
///
/// The closing quote has to be there: a name that is built at runtime or
/// interpolated is not an "exact string literal", which is the whole
/// distinction row 2 of the table draws against row 1.
fn scan_literals(text: &str, quoted_prefix: &str, out: &mut BTreeSet<String>) {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while let Some(off) = text[i..].find(quoted_prefix) {
        let start = i + off;
        let body = start + quoted_prefix.len();
        let mut end = body;
        while end < bytes.len() && is_flag_byte(bytes[end]) {
            end += 1;
        }
        if end > body && bytes.get(end) == Some(&b'"') {
            // `start + 1` drops the opening quote, `end` stops before the
            // closing one — the `tr -d '"'` in the documented recipe.
            out.insert(text[start + 1..end].to_string());
        }
        i = body;
    }
}

/// Every `*.md` a public reader can reach from the top of the tree: the
/// workspace root, all of `gc/`, and the top level of `docs/`.
///
/// The root and `docs/` walks are deliberately non-recursive, which is what
/// keeps `docs/internal` — history, and allowed to describe a world that has
/// since changed — out of scope without needing a special case.
fn published_markdown(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    markdown_here(root, &mut out);
    markdown_here(&root.join("docs"), &mut out);
    markdown_tree(&root.join("gc"), &mut out);
    out.sort();
    out.dedup();
    out
}

fn markdown_here(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(".md") && !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            out.push(path);
        }
    }
}

fn markdown_tree(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if !SKIPPED_DIRS.contains(&name) && !name.starts_with('.') {
                markdown_tree(&path, out);
            }
        } else if name.ends_with(".md") {
            out.push(path);
        }
    }
}

fn display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

// ---------------------------------------------------------------------------
// 1. The `zgc` Cargo feature default
// ---------------------------------------------------------------------------

/// No published document may describe `zgc` as a default-off Cargo feature
/// while `gc/Cargo.toml` has it in `default`.
///
/// `gc/Cargo.toml` is the authority; `ARCHITECTURE.md`, `CONTRIBUTING.md`,
/// `ROADMAP.md` and `gc/README.md` were the four documents that told readers a
/// stock build did not contain ZGC, for three weeks after `default = ["zgc"]`
/// landed on 2026-08-10. That is not a cosmetic error: a reader who believes it
/// reasons about the wrong collector when triaging a heap bug on a default
/// build, and two sessions did.
///
/// # The day the default legitimately changes
///
/// The test mirrors instead of skipping. If `zgc` leaves the `default` list,
/// the banned phrases become the two "default-on" spellings instead, so the
/// documents are still held to the manifest — just in the other direction. A
/// guard that passes vacuously in one branch of a boolean is half a guard, and
/// the half that goes missing is always the one nobody is thinking about on the
/// day the flip happens.
///
/// The mirror is narrow on purpose. There is no way to enumerate every phrasing
/// that could assert a default, so it pins the exact wording the tree already
/// uses rather than pretending to more coverage than it has; the branch that is
/// live today is the one that was actually wrong.
#[test]
fn no_published_document_calls_zgc_a_default_off_feature() {
    let manifest = read("gc/Cargo.toml");
    let features = manifest
        .split_once("\n[features]")
        .unwrap_or_else(|| {
            panic!(
                "gc/Cargo.toml has no [features] section, so this test cannot \
                 tell what the `zgc` default is. It fails rather than skips: a \
                 guard that stops finding its subject reports success while \
                 checking nothing."
            )
        })
        .1;

    // The first `default = …` line of `[features]`, stopping at the next
    // section header. Comment lines start with `#`, so a `[` in the first
    // column is unambiguously the end of the table.
    let mut default_line: Option<&str> = None;
    for line in features.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            break;
        }
        if trimmed.starts_with("default") && trimmed.contains('=') {
            default_line = Some(trimmed);
            break;
        }
    }
    let zgc_is_default = default_line.is_some_and(|l| l.contains("\"zgc\""));

    let (banned, truth): ([&str; 2], String) = if zgc_is_default {
        (
            ["default-off `zgc`", "default-off zgc"],
            format!(
                "`zgc` IS a default feature — gc/Cargo.toml [features] says \
                 `{}`, and vm/src/config.rs defaults `gc_algorithm` to \
                 `GcAlgorithm::Zgc`. A stock build has ZGC and uses it.",
                default_line.unwrap_or("").trim()
            ),
        )
    } else {
        (
            ["default-on `zgc`", "default-on zgc"],
            format!(
                "`zgc` is NOT a default feature — gc/Cargo.toml [features] says \
                 `{}`. A stock build does not enable it.",
                default_line.unwrap_or("(no `default = […]` line at all)").trim()
            ),
        )
    };

    let root = workspace_root();
    let pages = published_markdown(&root);
    assert!(
        pages.len() >= 10,
        "only {} markdown pages found under {} — the walk found essentially \
         nothing and this test was about to approve anything",
        pages.len(),
        root.display()
    );

    let mut offences: Vec<String> = Vec::new();
    for page in &pages {
        let Ok(text) = std::fs::read_to_string(page) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            for phrase in banned {
                if line.contains(phrase) {
                    offences.push(format!(
                        "{}:{}: {}",
                        display(&root, page),
                        n + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        offences.is_empty(),
        "a published document describes the `zgc` Cargo feature the wrong way \
         round.\n\n  The manifest is the authority: {truth}\n\n  Offending \
         lines (fix the prose, not gc/Cargo.toml):\n  {}\n\n  \
         `docs/internal` is exempt and not scanned — those pages are history \
         and are allowed to record what was once true.",
        offences.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// 2. ARCHITECTURE.md's total line count
// ---------------------------------------------------------------------------

/// The document's own recipe, re-run here, must land within 3% of the number
/// the document states.
///
/// 3% of two million lines is sixty thousand, which is several months of
/// growth and about a sixteenth of the 49% error this test exists to catch.
/// The figure is documented as rounded ("roughly"), so pinning it exactly would
/// redden the build on any commit that adds a line — and a test that fails for
/// a reason nobody caused is a test that gets deleted, not fixed.
///
/// The stated number is *parsed out of the prose*, never written down here.
/// A copy of it in this file would be a second hand-maintained number with
/// nothing checking it, which is the exact disease.
#[test]
fn architecture_total_loc_matches_its_own_recipe() {
    const TOLERANCE_PPM: u64 = 30_000; // 3%
    const RECIPE: &str = "| xargs -0 cat | wc -l";

    let doc = read("ARCHITECTURE.md");
    assert!(
        doc.contains(RECIPE),
        "ARCHITECTURE.md no longer contains the `{RECIPE}` reproduction \
         recipe this test re-derives. Failing rather than skipping: if the \
         recipe moved, the number beside it needs a human, and a guard that \
         quietly stops finding its subject is worse than no guard."
    );

    let members = workspace_members();
    let root = workspace_root();
    let actual: u64 = members.iter().map(|m| count_rs_lines(&root.join(m))).sum();

    // "…across the 22 workspace member crates" — the same sentence carries the
    // member count, and it is derived from the same list.
    let stated_members = number_after(&doc, "Rust LoC across the ").unwrap_or_else(|| {
        panic!(
            "ARCHITECTURE.md no longer says \"… Rust LoC across the N workspace \
             member crates\" — the sentence this test keys off has been \
             reworded"
        )
    });
    assert_eq!(
        stated_members as usize,
        members.len(),
        "ARCHITECTURE.md says the workspace has {} member crates; the root \
         Cargo.toml `[workspace] members` list names {}. Update the sentence \
         beginning \"The VM is the core of the project\".",
        stated_members,
        members.len()
    );

    let prose = number_before(&doc, " Rust LoC across the ").unwrap_or_else(|| {
        panic!("ARCHITECTURE.md's \"~N Rust LoC across the …\" figure did not parse")
    });
    let recipe_claim = number_after(&doc, "which reports roughly ").unwrap_or_else(|| {
        panic!("ARCHITECTURE.md's \"which reports roughly N lines\" figure did not parse")
    });

    for (what, claimed) in [
        ("the \"~N Rust LoC\" figure in the `vm` section", prose),
        ("the \"which reports roughly N lines\" figure", recipe_claim),
    ] {
        let ppm = deviation_ppm(claimed, actual);
        assert!(
            ppm <= TOLERANCE_PPM,
            "ARCHITECTURE.md: {what} is stale.\n  claimed:   {}\n  actual:    \
             {}\n  deviation: {} (tolerance {})\n\n  Re-derive with the \
             document's own recipe:\n    find {} -name '*.rs' -type f -not \
             -path '*/target/*' -not -path '*/vendor/*' -print0 | xargs -0 cat \
             | wc -l\n\n  Both this figure and the per-crate table below it \
             come from that one command — change them together.",
            commas(claimed),
            commas(actual),
            percent(ppm),
            percent(TOLERANCE_PPM),
            members.join(" ")
        );
    }
}

// ---------------------------------------------------------------------------
// 3. ARCHITECTURE.md's per-crate LoC table
// ---------------------------------------------------------------------------

/// Every row of the "Rough size distribution" table, re-derived.
///
/// The total being right does not make the rows right: when the total stood at
/// 1,350,000 the rows underneath it were independently wrong by 30-140% each,
/// and a reader deciding which crate to open is steered by the rows, not the
/// total.
///
/// # Tolerance
///
/// A row passes if it is within 5% **or** within 1,000 lines, whichever is
/// kinder. The 1,000-line floor is not slack — the table is written to the
/// nearest thousand, so a 12,000-line crate carries up to ±4% of pure rounding
/// before anybody has written a line of code. 5% on top of that leaves room for
/// ordinary growth while still catching drift an order of magnitude smaller
/// than the drift that actually happened.
///
/// The `remaining 9` / `< 13,000 each` cell is skipped: it is a bound, not a
/// count, and this test does not check bounds.
#[test]
fn architecture_per_crate_loc_table_matches_reality() {
    const TOLERANCE_PPM: u64 = 50_000; // 5%
    const TOLERANCE_FLOOR: u64 = 1_000; // the table is rounded to the nearest 1,000
    const HEADER: &str = "| Crate | LoC | Crate | LoC |";

    let doc = read("ARCHITECTURE.md");
    let at = doc.find(HEADER).unwrap_or_else(|| {
        panic!(
            "ARCHITECTURE.md no longer contains the `{HEADER}` table header. \
             Failing rather than skipping — if the table was reshaped, the \
             numbers in it need re-deriving by hand."
        )
    });

    let mut claims: Vec<(String, u64)> = Vec::new();
    for line in doc[at..].lines().skip(1) {
        if !line.starts_with('|') {
            break;
        }
        let cells: Vec<&str> = line
            .trim()
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect();
        if cells.len() != 4 {
            continue; // the `|---|---:|` separator row
        }
        for pair in cells.chunks(2) {
            let name = pair[0];
            // A crate cell is backticked. `remaining 9` is not, and is skipped
            // here rather than tripping `parse_count` on `< 13,000 each`.
            let Some(name) = name.strip_prefix('`').and_then(|n| n.strip_suffix('`')) else {
                continue;
            };
            let Some(loc) = parse_count(pair[1]) else {
                continue;
            };
            claims.push((name.to_string(), loc));
        }
    }

    assert!(
        claims.len() >= 10,
        "only {} rows parsed out of the ARCHITECTURE.md size table — the row \
         format moved under this test and it was about to check nothing",
        claims.len()
    );

    let members = workspace_members();
    let unknown: Vec<&str> = claims
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| !members.iter().any(|m| m.as_str() == *n))
        .collect();
    assert!(
        unknown.is_empty(),
        "the ARCHITECTURE.md size table names crates that are not in the root \
         Cargo.toml `[workspace] members` list: {unknown:?}. Either the crate \
         was renamed or the table is describing a layout that no longer exists."
    );

    let root = workspace_root();
    let mut rows: Vec<(u64, String, u64, u64)> = claims
        .iter()
        .map(|(name, claimed)| {
            let actual = count_rs_lines(&root.join(name));
            (deviation_ppm(*claimed, actual), name.clone(), *claimed, actual)
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));

    let stale: Vec<&(u64, String, u64, u64)> = rows
        .iter()
        .filter(|(ppm, _, claimed, actual)| {
            *ppm > TOLERANCE_PPM && claimed.abs_diff(*actual) > TOLERANCE_FLOOR
        })
        .collect();

    let mut report = String::new();
    for (ppm, name, claimed, actual) in &rows {
        let bad = *ppm > TOLERANCE_PPM && claimed.abs_diff(*actual) > TOLERANCE_FLOOR;
        report.push_str(&format!(
            "\n  {} {:<26} claimed {:>10}  actual {:>10}  off by {}",
            if bad { "STALE" } else { "  ok " },
            name,
            commas(*claimed),
            commas(*actual),
            percent(*ppm)
        ));
    }

    assert!(
        stale.is_empty(),
        "{} row(s) of the ARCHITECTURE.md \"Rough size distribution\" table are \
         stale (tolerance: {} or {} lines, whichever is kinder), worst \
         first:{}\n\n  Re-derive one crate with:\n    find <crate> -name \
         '*.rs' -type f -not -path '*/target/*' -not -path '*/vendor/*' \
         -print0 | xargs -0 cat | wc -l\n\n  The paragraph above the table \
         quotes the same measurement — change both.",
        stale.len(),
        percent(TOLERANCE_PPM),
        commas(TOLERANCE_FLOOR),
        report
    );
}

// ---------------------------------------------------------------------------
// 4. flag-inventory.md's two hand-written count rows
// ---------------------------------------------------------------------------

/// The first two rows of "Where the surface stands" are the only rows in that
/// document nothing generates, and they are the rows that drifted.
///
/// They read 692 / 658 from 2026-08-06 to 2026-09-01 while the true figures
/// were 1,056 / 993 — a gap of 350 wide enough that the declared count in the
/// row beneath them appeared to exceed the number of flag names in the source,
/// which cannot happen. The document's own header says why: "generated" is
/// exactly the label that stops anyone reading a file sceptically, and these
/// two rows inherited that trust without ever having a generator.
///
/// Asserted **exactly**, with no tolerance. Unlike the LoC figures these are
/// not rounded estimates of a growing quantity — each is the output of a
/// `sort -u | wc -l` over a fixed pattern, so "close" has no meaning and any
/// difference is a difference.
///
/// # The recipe, and who writes these rows now
///
/// Both rows are written by `tools/flag-census/render-inventory.sh` as of
/// 2026-09-02, so the way to fix a failure here is to REGENERATE, not to
/// hand-edit — the hand-edit is what made these two the rows that drift. This
/// test still scans independently: the generator and the test walk the tree
/// with separate code, and the point is that they agree.
///
/// Row 1's recipe used to grep `.` unrestricted while this test skipped
/// `target/`, `vendor/` and dotted directories — harmless only for as long as
/// no `build.rs` emitted a name into `OUT_DIR`, and misleading to anyone who
/// ran it in a built tree. It now carries the same exclusions. The one this
/// test applies that the recipe cannot express is the internal docs tree,
/// which `collect_rust` also skips; no `*.rs` lives there today, so the two
/// agree. (Spelled in words rather than as a path because
/// `no_source_file_links_into_docs_internal` forbids that path outside the
/// tree it names, and it cannot tell a directory reference from a citation.)
#[test]
fn flag_inventory_surface_counts_are_current() {
    const SECTION: &str = "## Where the surface stands";
    const ROW1: &str = "| distinct `CRATONVM_*` identifiers appearing anywhere in Rust source |";
    const ROW2: &str = "| exact string literals (i.e. actually named by code, not prose) |";

    let doc = read("docs/config/flag-inventory.md");
    let section = doc
        .split_once(SECTION)
        .unwrap_or_else(|| {
            panic!(
                "docs/config/flag-inventory.md no longer has a `{SECTION}` \
                 section. Failing rather than skipping — the counts this test \
                 guards have to live somewhere, and if the section was renamed \
                 this test has to follow it."
            )
        })
        .1;

    let claimed = |row: &str| -> u64 {
        let line = section
            .lines()
            .find(|l| l.starts_with(row))
            .unwrap_or_else(|| {
                panic!(
                    "the \"Where the surface stands\" table has no row starting \
                     `{row}` — it was reworded, and this test cannot tell \
                     whether the number moved with it"
                )
            });
        let cell = line.trim().trim_matches('|').rsplit('|').next().unwrap_or("");
        parse_count(cell).unwrap_or_else(|| {
            panic!("the count cell of `{row}` did not parse as a number: {line:?}")
        })
    };

    let root = workspace_root();
    let prefix = flag_prefix();
    let quoted_prefix = format!("\"{prefix}");

    // Row 1 — identifiers anywhere in Rust source, whole repository.
    let mut files = Vec::new();
    collect_rust(&root, &root, &mut files);
    assert!(
        files.len() >= 100,
        "only {} Rust files found under {} — the walk found essentially \
         nothing and both counts below would be meaningless",
        files.len(),
        root.display()
    );
    let mut identifiers: BTreeSet<String> = BTreeSet::new();
    for path in &files {
        if let Ok(text) = std::fs::read_to_string(path) {
            scan_identifiers(&text, &prefix, &mut identifiers);
        }
    }

    // Row 2 — exact string literals, `<member>/src` only.
    let mut literals: BTreeSet<String> = BTreeSet::new();
    for member in workspace_members() {
        let src = root.join(&member).join("src");
        if !src.is_dir() {
            continue;
        }
        let mut src_files = Vec::new();
        collect_rust(&src, &root, &mut src_files);
        for path in &src_files {
            if let Ok(text) = std::fs::read_to_string(path) {
                scan_literals(&text, &quoted_prefix, &mut literals);
            }
        }
    }

    let row1 = claimed(ROW1);
    assert_eq!(
        row1,
        identifiers.len() as u64,
        "docs/config/flag-inventory.md, \"Where the surface stands\" row 1 is \
         stale.\n  claimed: {}\n  actual:  {}\n\n  Regenerate with:\n    \
         {ROW1_RECIPE}\n\n  This row and the one below it are the only ones in \
         that document nothing generates, which is why they are the ones that \
         drifted by 350.",
        commas(row1),
        commas(identifiers.len() as u64)
    );

    let row2 = claimed(ROW2);
    assert_eq!(
        row2,
        literals.len() as u64,
        "docs/config/flag-inventory.md, \"Where the surface stands\" row 2 is \
         stale.\n  claimed: {}\n  actual:  {}\n\n  Regenerate with:\n    \
         {ROW2_RECIPE}\n\n  Row 2 counts quoted literals under `<member>/src` \
         only, so it is expected to be smaller than row 1 — a name reached \
         only through prose or a runtime-built key is in row 1 and not here.",
        commas(row2),
        commas(literals.len() as u64)
    );
}
