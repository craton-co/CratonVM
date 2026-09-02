// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A `docs/….md` citation in a Rust comment must point at a file that is still
//! there.
//!
//! # Why a dead link is a bug, not untidiness
//!
//! Most citations in this tree hang off a workaround: a receiver-shape check, a
//! forced native, a pin across a GC-capable call. The comment says *why* the
//! code is shaped that way and the link is the evidence. When the referenced
//! report moves — and reports move constantly here, `docs/known-issues/X.md`
//! becoming `docs/internal/fixed-suite-bugs/…/X-FIXED.md` the day the bug is
//! closed — the link rots while the workaround stays load-bearing. A reader who
//! follows it, finds nothing, and concludes the workaround is obsolete deletes
//! live code. That has already happened here: the `h2-bnf` String fast path was
//! landed, measured, and then left dead behind a gate mismatch for weeks
//! because the reasoning that justified it was no longer reachable.
//!
//! # What counts as a failure
//!
//! Exactly one thing: a citation whose file is **not** at the cited path, and
//! whose basename identifies **exactly one** page under `docs/` somewhere else.
//! That is the relocatable case — the answer is known, so leaving it dead is a
//! choice rather than an open question.
//!
//! Deliberately *not* failures, because this guard cannot answer them and a
//! guard that guesses is worse than none:
//!
//! * a basename that matches several pages (`README.md`, `roadmap.md`) — the
//!   right target needs a human who knows which one the comment meant;
//! * a basename that matches nothing — the page was deleted, or the citation
//!   is a placeholder (`docs/internal/x.md`) or a glob (`…/ES-HANG-*`). Whether
//!   the comment should be rewritten or dropped is an editorial call.
//!
//! # Basename matching, and the `-FIXED` rename
//!
//! An exact basename match wins outright. Only if nothing matches exactly is a
//! trailing status suffix ([`STATUS_SUFFIXES`], optionally followed by a
//! `-YYYYMMDD` stamp) normalised away on both sides, which is what connects
//! `bug-h2-timezone-….md` to `bug-h2-timezone-….md`'s `-FIXED` copy. Exact-first
//! matters: when both `foo.md` and `foo-FIXED.md` exist, normalising first would
//! see two candidates and give up on a citation that is not ambiguous at all.
//!
//! This rule was not taken on faith. It was cross-checked against `git log
//! --diff-filter=R` over `docs/`: of the 323 dead citations where git's rename
//! history and this basename rule both produced an answer, they agreed on 323.
//!
//! # The two citation shapes
//!
//! Long paths get wrapped across a comment break, so the scanner has to rejoin
//! them before it can judge them:
//!
//! ```text
//! // See docs/known-issues/foo.md.                 <- whole path, one line
//!
//! // ... (docs/known-issues/                       <- wrapped: the break may
//! //   foo.md).                                       fall after `/` or `-`
//! ```
//!
//! A break is only honoured directly after `/` or `-`, which is where every
//! real wrapped citation in this tree breaks. Without that restriction a
//! comment ending in `see docs/known-issues/` followed by an unrelated line
//! mentioning `foo.md` would be spliced into one bogus path.
//!
//! # `docs/internal/` is not citable from here
//!
//! [`no_source_file_links_into_docs_internal`] holds the second rule: no
//! tracked file outside `docs/internal/` may contain a `docs/internal/…` path.
//! Those records are not published, so such a path is a link that no public
//! reader can follow — the same dead-end this file exists to prevent, one step
//! further out.
//!
//! Citations to an internal record therefore drop the prefix and keep the
//! record's own path (`fixed-suite-bugs/foo-FIXED.md`), which is what it is
//! called relative to the internal tree's own root. Anyone holding those docs
//! can still find it; nobody else is sent to a path that is not there.
//!
//! Bare prose mentions of the directory (`move its document under
//! docs/internal`, with no trailing slash) are deliberately still allowed:
//! they describe where records go, which is documented process, not a link to
//! one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Directory names never scanned: build output, VCS metadata, and the Java
/// application harnesses under `apps/` (not Rust sources).
const SKIPPED_DIRS: &[&str] = &["target", ".git", "apps", "node_modules"];

/// Status words a report's filename picks up when its state changes. Stripped
/// only as a *trailing* token, optionally followed by a `-YYYYMMDD` stamp, and
/// only when an exact basename match has already failed.
const STATUS_SUFFIXES: &[&str] = &[
    "FIXED",
    "CLOSED",
    "OPEN",
    "SUPERSEDED",
    "RETIRED",
    "RETRACTED",
    "WITHDRAWN",
    "RESOLVED",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("types/ always has a workspace root above it")
        .to_path_buf()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
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

/// Every `.md` under `docs/`, as a slash-separated path relative to the
/// workspace root.
fn doc_pages(dir: &Path, root: &Path, out: &mut Vec<String>) {
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
                doc_pages(&path, root, out);
            }
        } else if name.ends_with(".md") {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

/// `foo-FIXED-20260803.md` -> `foo`. Idempotent past the first strip, so a name
/// that picked up two status words loses both.
fn normalized_stem(basename: &str) -> String {
    let mut stem = basename.strip_suffix(".md").unwrap_or(basename);
    loop {
        let before = stem;
        // An optional trailing `-YYYYMMDD` sits outside the status word.
        let undated = match stem.rsplit_once('-') {
            Some((head, tail)) if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) => {
                head
            }
            _ => stem,
        };
        for suffix in STATUS_SUFFIXES {
            if let Some(head) = undated.strip_suffix(suffix) {
                if let Some(head) = head.strip_suffix('-') {
                    stem = head;
                    break;
                }
            }
        }
        if stem == before {
            return stem.to_string();
        }
    }
}

fn basename(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, b)| b)
}

fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-' | b'/')
}

/// If a citation continues on the next line, the index just past the break.
///
/// Two continuations exist: a comment marker (`//`, `///`, `//!`, or a block
/// comment's `*`) and a Rust string literal's `\`-at-end-of-line. Backticks on
/// either side of a comment break are absorbed — several citations here have
/// their two halves separately code-quoted.
fn skip_break(b: &[u8], from: usize, last: Option<u8>) -> Option<usize> {
    if !matches!(last, Some(b'/') | Some(b'-')) {
        return None;
    }
    let mut i = from;
    // A string-literal continuation takes the backslash first and allows
    // nothing before it.
    if b.get(i) == Some(&b'\\') {
        i += 1;
        i = skip_newline(b, i)?;
        while matches!(b.get(i), Some(b' ') | Some(b'\t')) {
            i += 1;
        }
        return Some(i);
    }
    if b.get(i) == Some(&b'`') {
        i += 1;
    }
    while matches!(b.get(i), Some(b' ') | Some(b'\t')) {
        i += 1;
    }
    i = skip_newline(b, i)?;
    while matches!(b.get(i), Some(b' ') | Some(b'\t')) {
        i += 1;
    }
    if b.get(i) == Some(&b'/') && b.get(i + 1) == Some(&b'/') {
        i += 2;
        if matches!(b.get(i), Some(b'/') | Some(b'!')) {
            i += 1;
        }
    } else if b.get(i) == Some(&b'*') {
        i += 1;
    } else {
        return None;
    }
    while matches!(b.get(i), Some(b' ') | Some(b'\t')) {
        i += 1;
    }
    if b.get(i) == Some(&b'`') {
        i += 1;
    }
    Some(i)
}

/// Past a `\n` or `\r\n` at `i`, or `None` if there is no line break there.
fn skip_newline(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i) {
        Some(b'\n') => Some(i + 1),
        Some(b'\r') if b.get(i + 1) == Some(&b'\n') => Some(i + 2),
        _ => None,
    }
}

/// Consume a path starting at `start`, rejoining comment breaks, and return
/// `(path-through-its-last-".md", index just past it)`.
fn consume_path(b: &[u8], start: usize) -> Option<(String, usize)> {
    let mut i = start;
    let mut logical = String::new();
    loop {
        match b.get(i) {
            Some(&c) if is_path_byte(c) => {
                logical.push(c as char);
                i += 1;
            }
            _ => match skip_break(b, i, logical.as_bytes().last().copied()) {
                Some(next) => i = next,
                None => break,
            },
        }
    }
    // Greedy consumption overshoots any trailing `.`/`)`/backtick, and can run
    // past the end of a wrapped path into the prose after it. Cutting at the
    // LAST `.md` is what the equivalent regex's backtracking does.
    let end = logical.rfind(".md")?;
    Some((logical[..end + 3].to_string(), start + end + 3))
}

/// Every `….md` path in `text`, rejoined across comment breaks, with the
/// 1-based line it starts on.
///
/// Both citation styles come out of here: a published one roots at `docs/`, an
/// internal one is a bare path relative to the internal tree's own root. A run
/// only starts where a path character follows a non-path character, so the
/// tail of a longer path is never mistaken for a citation of its own.
fn md_paths(text: &str) -> Vec<(String, usize)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut prev_is_path = false;
    while i < b.len() {
        let here_is_path = is_path_byte(b[i]);
        if here_is_path && !prev_is_path {
            if let Some((path, end)) = consume_path(b, i) {
                out.push((path, text[..i].matches('\n').count() + 1));
                i = end.max(i + 1);
                prev_is_path = true;
                continue;
            }
        }
        prev_is_path = here_is_path;
        i += 1;
    }
    out
}

/// Where a dead citation's page actually lives, if that is knowable.
fn relocated_to<'a>(
    cited: &str,
    exact: &'a BTreeMap<String, Vec<String>>,
    loose: &'a BTreeMap<String, Vec<String>>,
) -> Option<&'a String> {
    let base = basename(cited);
    match exact.get(base).map(Vec::as_slice) {
        Some([only]) => return (only != cited).then_some(only),
        // Several pages share this basename: which one the comment meant is a
        // human's call, not this guard's.
        Some(_) => return None,
        None => {}
    }
    match loose.get(&normalized_stem(base)).map(Vec::as_slice) {
        Some([only]) => (only != cited).then_some(only),
        _ => None,
    }
}

/// File extensions this repo keeps prose or source in. Anything else is data
/// or a build artefact and is not worth reading to look for a citation.
const TEXT_EXTENSIONS: &[&str] = &[
    ".rs", ".md", ".toml", ".py", ".sh", ".ps1", ".java", ".tsv", ".yml", ".yaml", ".json", ".txt",
];

/// Every tracked text file outside `docs/internal/`.
///
/// The list comes from `git ls-files`, not from a directory walk, because the
/// rule is about what the repository publishes and "tracked" is exactly that.
/// A walk cannot tell the difference: `apps/` holds suite checkouts and local
/// bug-report scratch that no one committed, so a walk fails on one machine and
/// passes on another depending on what happens to be lying there. It is also
/// the difference between a 90-second test and a fast one.
///
/// `apps/` itself must stay in scope — it is skipped by [`SKIPPED_DIRS`] for
/// the Rust-source walk, but its tracked READMEs, run scripts and result tables
/// cite records like anything else, and four `docs/internal/` paths were found
/// in `apps/hib-suite-runner/known-benign-aborts.tsv`.
fn tracked_text_files(root: &Path) -> Vec<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .expect("`git ls-files` must run: this guard defines \"in the repo\" as \"tracked\"");
    assert!(
        out.status.success(),
        "`git ls-files` failed in {} — failing rather than checking nothing",
        root.display()
    );
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|rel| !rel.is_empty())
        .filter(|rel| !rel.starts_with("docs/internal/"))
        .filter(|rel| TEXT_EXTENSIONS.iter().any(|e| rel.ends_with(e)))
        .map(|rel| root.join(rel))
        .collect()
}

/// The target of every `[label](target)` on `line`, skipping URLs and anchors.
fn markdown_link_targets(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // `](` is the only anchor that matters; a `(` alone is ordinary prose.
        if bytes[i] == b']' && bytes.get(i + 1) == Some(&b'(') {
            let start = i + 2;
            if let Some(len) = line[start..].find(')') {
                let target = line[start..start + len].split('#').next().unwrap_or("");
                if !target.is_empty()
                    && !target.starts_with("http://")
                    && !target.starts_with("https://")
                    && !target.starts_with("mailto:")
                    && !target.contains(char::is_whitespace)
                {
                    out.push(target);
                }
                i = start + len;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// `target` resolved against `dir`, both slash-separated and repo-relative.
fn resolve_relative(dir: &str, target: &str) -> String {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// The one kind of line that may spell the internal prefix in full, and why.
///
/// A **filesystem path that code uses to LOCATE a file at run time** is not a
/// citation. This guard is a text scan and cannot tell the two apart, and the
/// difference matters in exactly the wrong direction: dropping the prefix from
/// a citation repairs a dead link, while dropping it from a probe path breaks
/// the lookup — today, in the tree where `docs/internal/` still exists.
///
/// Each row is `(file, the exact trimmed line, why)`. The match is on the LINE,
/// not the file, so a new `docs/internal/` line in the same file is still
/// caught. [`the_not_a_citation_rows_are_all_live`] stops the row from becoming
/// a standing permission by requiring the line to still be there.
///
/// Nothing else belongs here. A citation that is merely awkward to reword is
/// still a citation.
const NOT_A_CITATION: &[(&str, &str, &str)] = &[
    (
        "vm/tests/jck_conformance.rs",
        "format!(\"{manifest_dir}/../docs/internal/gaps/jdk-regression-baseline.md\"),",
        "`baseline_document()`'s FIRST candidate path, probed with `read_to_string` and falling back to the repo-root `gaps/` copy. It is what keeps the gate working while `docs/internal/` is removed from history, so the literal is a path being tolerated, not a link being offered.",
    ),
];

/// A `NOT_A_CITATION` row whose line is gone is permission nobody needs — the
/// same defect the resolve-bypass allowlist's dead-row test names.
#[test]
fn the_not_a_citation_rows_are_all_live() {
    let root = workspace_root();
    let mut stale = Vec::new();
    for (file, line, reason) in NOT_A_CITATION {
        let present = std::fs::read_to_string(root.join(file))
            .map(|t| t.lines().any(|l| l.trim() == *line))
            .unwrap_or(false);
        if !present {
            stale.push(format!("{file}: the exempted line is gone ({reason})"));
        }
    }
    assert!(
        stale.is_empty(),
        "{} exemption(s) no longer describe the tree. Delete them — an \
         exemption for a line nobody wrote is permission for the next one to \
         appear:\n  {}",
        stale.len(),
        stale.join("\n  ")
    );
}

#[test]
fn no_source_file_links_into_docs_internal() {
    let root = workspace_root();
    let internal = root.join("docs").join("internal");
    assert!(
        internal.is_dir(),
        "{} is gone — if the internal records moved out for good, delete this \
         test rather than letting it pass by finding nothing to check",
        internal.display()
    );

    let files = tracked_text_files(&root);
    assert!(
        files.len() > 1000,
        "only found {} tracked text files in {} — the listing is not reaching \
         the repository, so this guard would pass vacuously",
        files.len(),
        root.display()
    );

    let this_file = Path::new(file!()).file_name().expect("test file name");
    let mut offenders = Vec::new();
    for path in &files {
        // This file's module docs have to spell the forbidden prefix out.
        if path.file_name() == Some(this_file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if !text.contains("docs/internal/") {
            continue;
        }
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        for (n, line) in text.lines().enumerate() {
            if !line.contains("docs/internal/") {
                continue;
            }
            // A run-time filesystem probe is not a citation; see NOT_A_CITATION.
            if NOT_A_CITATION
                .iter()
                .any(|(f, l, _)| *f == rel && *l == line.trim())
            {
                continue;
            }
            offenders.push(format!("{rel}:{}\n    {}", n + 1, line.trim()));
        }
    }

    // A markdown link can reach the same tree without ever spelling it:
    // `../../internal/foo.md` from inside docs/ lands in docs/internal/ too.
    for path in &files {
        if path.extension().and_then(|e| e.to_str()) != Some("md")
            || path.file_name() == Some(this_file)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
        for (n, line) in text.lines().enumerate() {
            for target in markdown_link_targets(line) {
                if resolve_relative(dir, target).starts_with("docs/internal/") {
                    offenders.push(format!("{rel}:{}\n    -> {target}", n + 1));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "{} line(s) outside docs/internal/ carry a docs/internal/ path. Those \
         records are not published, so the path is a link a public reader \
         cannot follow. Drop the `docs/internal/` prefix and keep the rest — \
         the record's path relative to the internal tree's own root.\n\n{}\n",
        offenders.len(),
        offenders.join("\n\n")
    );
}

#[test]
fn every_relocatable_doc_citation_points_at_the_page() {
    let root = workspace_root();

    let mut pages = Vec::new();
    doc_pages(&root.join("docs"), &root, &mut pages);
    assert!(
        pages.len() > 1000,
        "only found {} pages under {}/docs — the walk is not reaching the \
         documentation, so this guard would pass vacuously",
        pages.len(),
        root.display()
    );

    let mut exact: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut loose: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for page in &pages {
        exact
            .entry(basename(page).to_string())
            .or_default()
            .push(page.clone());
        loose
            .entry(normalized_stem(basename(page)))
            .or_default()
            .push(page.clone());
    }

    // The internal tree is indexed on its own terms, because a citation of an
    // internal record is a path relative to *its* root, not to the workspace.
    let internal_root = root.join("docs").join("internal");
    let mut internal_pages = Vec::new();
    doc_pages(&internal_root, &internal_root, &mut internal_pages);
    let internal_top: std::collections::BTreeSet<String> = std::fs::read_dir(&internal_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    assert!(
        internal_top.len() > 3,
        "found {} directories under {} — without them no prefix-less citation \
         can be anchored, so that half of this guard would check nothing",
        internal_top.len(),
        internal_root.display()
    );
    let mut internal_exact: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut internal_loose: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for page in &internal_pages {
        internal_exact
            .entry(basename(page).to_string())
            .or_default()
            .push(page.clone());
        internal_loose
            .entry(normalized_stem(basename(page)))
            .or_default()
            .push(page.clone());
    }

    let mut sources = Vec::new();
    rust_sources(&root, &mut sources);
    assert!(
        sources.len() > 500,
        "only found {} Rust sources under {} — the walk is not reaching the \
         workspace, so this guard would pass vacuously",
        sources.len(),
        root.display()
    );

    let this_file = Path::new(file!()).file_name().expect("test file name");
    let mut live = 0usize;
    let mut live_internal = 0usize;
    let mut dead = Vec::new();
    for path in &sources {
        // This file's module docs spell out example citations, including
        // deliberately dead ones.
        if path.file_name() == Some(this_file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if !text.contains(".md") {
            continue;
        }
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        for (cited, line) in md_paths(&text) {
            if let Some(rest) = cited.strip_prefix("docs/") {
                let _ = rest;
                if root.join(&cited).is_file() {
                    live += 1;
                } else if let Some(target) = relocated_to(&cited, &exact, &loose) {
                    dead.push(format!(
                        "{rel}:{line}\n    cites {cited}\n    moved to {target}"
                    ));
                }
                continue;
            }
            // A prefix-less path is an internal citation only if it starts with
            // a directory the internal tree actually has. Without that anchor
            // any `.md` in prose whose basename happens to match an internal
            // record gets judged — `.claude/review-2026-05-24/vm-cli.md` is not
            // a citation of `reviews/fable-2026-06-10/vm-cli.md`.
            match cited.split_once('/') {
                Some((first, _)) if internal_top.contains(first) => {
                    if internal_root.join(&cited).is_file() {
                        live_internal += 1;
                    } else if let Some(target) =
                        relocated_to(&cited, &internal_exact, &internal_loose)
                    {
                        dead.push(format!(
                            "{rel}:{line}\n    cites {cited}\n    moved to {target}"
                        ));
                    }
                }
                // A bare filename carries no anchor at all, so it is counted
                // when it resolves and never accused when it does not.
                None if internal_root.join(&cited).is_file() => live_internal += 1,
                _ => {}
            }
        }
    }

    // A scanner that silently stopped matching would report zero dead links and
    // look like success. It has to be seen finding the live ones too — on both
    // sides, since the two now travel through different indexes.
    assert!(
        live > 400,
        "only {live} published citations resolved to a real page — the scanner \
         is not matching citations any more, so a zero dead-link result means \
         nothing"
    );
    assert!(
        live_internal > 400,
        "only {live_internal} internal citations resolved to a real record — \
         the prefix-less form is no longer being checked, so a zero dead-link \
         result means nothing"
    );

    assert!(
        dead.is_empty(),
        "{} doc citation(s) point at a path that no longer exists, and the page \
         they name is unambiguously somewhere else. Repoint each one — a reader \
         who follows a dead link concludes the workaround the comment explains \
         is obsolete.\n\n{}\n",
        dead.len(),
        dead.join("\n\n")
    );
}
