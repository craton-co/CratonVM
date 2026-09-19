// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! COVERAGE GATE for the READ-side slot-alias census — the sibling of
//! `layout_alias_coverage.rs`, and the reason a clean report from *either*
//! instrument can be read as coverage of only its own species.
//!
//! # The failure this exists to prevent
//!
//! `layout_alias` watches every native object **allocation** in the workspace
//! and compares a requested slot COUNT against the loaded class's. It cannot
//! see a native reading slot `k` of an object it never allocated, because there
//! is no allocation on that path, because a count cannot say "slot 0 is `mark`,
//! not `hb`", and because an in-bounds read of the wrong field never reaches
//! the `cratonvm::gc::guard` out-of-bounds list that its own doc comment
//! proposes as the fallback discriminator. W7-59-layout-detector-coverage.md
//! section 6 states all three and deliberately did not build the second
//! instrument; W7-69-read-side-alias-instrument.md builds it.
//!
//! The worked example: `bb_state` read slot 0 of a real
//! `java.nio.DirectByteBuffer` expecting the backing array `hb` and got
//! `java.nio.Buffer.mark` = -1. Perfectly in bounds. Invisible to both halves
//! of the documented intersection.
//!
//! # The argument this gate mechanises
//!
//! 1. There is exactly **one** read-side detector
//!    ([`there_is_exactly_one_read_side_detector`]) — two implementations of one
//!    primitive drift, then disagree, then the reader has to pick.
//! 2. It adds **no new flag** ([`the_read_side_census_adds_no_flag_of_its_own`])
//!    — a new `CRATONVM_*` name needs four files or `cargo test -p
//!    cratonvm-types` goes red, and the two censuses are two halves of one
//!    species anyway.
//! 3. Its slot→name oracle **walks the superclass chain**
//!    ([`the_slot_name_oracle_walks_the_superclass_chain`]). `declared_fields`
//!    is declared-only while `slot_index` is absolute, so an oracle that skips
//!    the walk answers "slot 0 does not exist" for the calibration case instead
//!    of "slot 0 is `mark`" — quieter, and wrong.
//! 4. Every observation is **gated and has no `else`**
//!    ([`every_read_side_observation_is_gated_and_observation_only`]) — a
//!    diagnostic that changes behaviour is a behaviour change in Compatible
//!    mode, which is contractually frozen.
//! 5. The **calibration site is still observed, before the read it is about**
//!    ([`the_calibration_site_is_still_observed_before_its_own_read`]). This is
//!    the one a hot-path cleanup breaks.
//! 6. A published [`SlotMap`](cratonvm_native_api::read_alias::SlotMap) is
//!    actually **published** ([`every_declared_slot_map_is_published`]) — a
//!    `SlotMap` that no registrar hands to `declare_slot_map` is dead data, and
//!    the sweep over it reports clean because it swept nothing. That is the
//!    vacuous-green shape this campaign keeps re-buying.
//! 7. The sweep is **actually invoked**
//!    ([`the_declared_slot_map_sweep_has_a_caller`]). Link 6 proves a map
//!    reaches the registry; it says nothing about whether anything ever reads
//!    the registry. From 2026-08-12 until W7-90-slot-map-sweep-caller.md
//!    `verify_declared_slot_maps` had **no caller at all** while seven
//!    `SlotMap`s were published to it, which is indistinguishable from a
//!    detector reporting all-clear — the dominant species of this campaign,
//!    found inside the instrument built to detect it.
//! 8. The launcher's trigger runs **after the workload**
//!    ([`the_post_main_sweep_runs_after_the_workload`]). Moved above the
//!    `main(String[])` invoke it becomes the registration-time check W7-69 §2
//!    rejected: most of the declared classes are not loaded yet, and
//!    `declared_fields` returning empty is indistinguishable from "this class
//!    has no fields".
//! 9. The self-terminating paths sweep too, and observe only
//!    ([`the_exit_paths_sweep_before_they_terminate`]). `System.exit` /
//!    `Runtime.exit` / `Runtime.halt` never reach the launcher's post-`main`
//!    line, and they are how every suite fixture this instrument is aimed at
//!    ends. **That link names its own three natives, so it cannot report that
//!    the population was wrong — and it was: `System.exit` is not the whole
//!    self-terminating population.** Link 10 covers the rest.
//! 10. The **Surefire fork's** exit paths sweep too
//!    ([`the_surefire_exit_paths_sweep_before_they_terminate`]). Four triples on
//!    `ForkedBooter` are registered in **both** shipping modes onto three bodies
//!    that terminate on their own and never reach `native_system_exit`, so a
//!    Surefire fork skipped the launcher trigger AND link 9's three natives —
//!    the exact fixture population W7-90 §2.2 named as its whole reason for
//!    existing. Closing three of four doors has looked identical to closing none
//!    in this repo before.
//!
//! Plus [`census`], which **prints** the read-side population and asserts only
//! that it is non-zero. Deliberately not a ratchet, for the same reason
//! `layout_alias_coverage.rs`'s census is not: a count over a population that
//! changes with every native added gets re-baselined on sight, and a gate people
//! re-baseline teaches them to re-baseline the whole file.
//!
//! # What this gate deliberately does NOT assert
//!
//! It does not require every constant-slot read in the workspace to carry an
//! expected field name. [`census`] counts 11,948 of them, and most read objects
//! the native allocated itself, where its slot map is the class's own truth. A
//! gate that failed on all of them would be muted on the day it landed. The
//! remainder is **printed** instead, so it is a visible number rather than a
//! silent one.

use std::fs;
use std::path::{Path, PathBuf};

const NATIVE_CRATES: &[&str] = &[
    "native-builtins",
    "native-io",
    "native-collections",
    "native-builtins-crypto",
    "native-builtins-security",
    "native-awt",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("native-api sits directly under the workspace root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_sources(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out
}

/// Blank out `//` and `/* */` comments, preserving byte offsets so line numbers
/// still work. Same routine as `layout_alias_coverage.rs`: a gate that counts a
/// spelling inside a doc comment reports findings that are not there, and a gate
/// with false positives gets muted.
fn strip_comments(src: &str) -> String {
    let bytes: Vec<char> = src.chars().collect();
    let mut out = bytes.clone();
    let (mut i, n) = (0usize, bytes.len());
    let (mut in_str, mut in_line) = (false, false);
    let mut block_depth = 0usize;
    while i < n {
        let c = bytes[i];
        if in_line {
            if c == '\n' {
                in_line = false;
            } else {
                out[i] = ' ';
            }
            i += 1;
        } else if block_depth > 0 {
            if c == '/' && i + 1 < n && bytes[i + 1] == '*' {
                out[i] = ' ';
                out[i + 1] = ' ';
                block_depth += 1;
                i += 2;
            } else if c == '*' && i + 1 < n && bytes[i + 1] == '/' {
                out[i] = ' ';
                out[i + 1] = ' ';
                block_depth -= 1;
                i += 2;
            } else {
                if c != '\n' {
                    out[i] = ' ';
                }
                i += 1;
            }
        } else if in_str {
            if c == '\\' {
                i += 2;
            } else {
                if c == '"' {
                    in_str = false;
                }
                i += 1;
            }
        } else if c == '"' {
            in_str = true;
            i += 1;
        } else if c == '/' && i + 1 < n && bytes[i + 1] == '/' {
            in_line = true;
            out[i] = ' ';
            out[i + 1] = ' ';
            i += 2;
        } else if c == '/' && i + 1 < n && bytes[i + 1] == '*' {
            block_depth = 1;
            out[i] = ' ';
            out[i + 1] = ' ';
            i += 2;
        } else {
            i += 1;
        }
    }
    out.into_iter().collect()
}

/// Byte offset of the `}` closing the `{` at `open`, string literals skipped.
fn match_brace(src: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (idx, ch) in src[open..].char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + idx);
                }
            }
            _ => {}
        }
    }
    None
}

/// The body of the first `fn <name>` in `src`, brace-matched.
///
/// Matches `fn <name>` followed by `(` **or** `<`, not just `(`:
/// `field_name_at` is generic (`fn field_name_at<O: SlotOracle + ?Sized>(`),
/// and a needle ending in `(` silently finds nothing there — a gate that
/// cannot locate its subject fails with "the function moved" and gets
/// re-baselined instead of read.
fn fn_body<'a>(src: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("fn {name}");
    let mut from = 0usize;
    let start = loop {
        let hit = src[from..].find(&needle)? + from;
        let next = src[hit + needle.len()..].chars().next();
        if matches!(next, Some('(') | Some('<')) {
            break hit;
        }
        from = hit + needle.len();
    };
    let open = src[start..].find('{')? + start;
    let close = match_brace(src, open)?;
    Some(&src[open..=close])
}

// ---------------------------------------------------------------------------
// Link 1 — one detector, not two
// ---------------------------------------------------------------------------

/// The classifier, the slot→name oracle, the dedup key and the channel live in
/// `native-api/src/read_alias.rs`, once.
///
/// The signature of "this file decides what the read-side census prints" is
/// that it emits a `wrong-field` direction. `layout_alias.rs` emits `over` and
/// `under` and must NOT appear here — the two vocabularies are separate on
/// purpose, and folding them is how a census ends up reporting a count where a
/// name was needed.
#[test]
fn there_is_exactly_one_read_side_detector() {
    let root = workspace_root();
    let mut owners: Vec<String> = Vec::new();
    for crate_dir in ["vm", "gc", "native-api", "types", "jit"]
        .iter()
        .chain(NATIVE_CRATES.iter())
    {
        for file in rust_sources(&root.join(crate_dir).join("src")) {
            let src = strip_comments(&fs::read_to_string(&file).unwrap_or_default());
            if src.contains("direction = \"wrong-field\"")
                || src.contains("direction = \"absent-slot\"")
            {
                owners.push(
                    file.strip_prefix(&root)
                        .unwrap_or(&file)
                        .display()
                        .to_string()
                        .replace('\\', "/"),
                );
            }
        }
    }
    assert_eq!(
        owners,
        vec!["native-api/src/read_alias.rs".to_string()],
        "the read-side alias census is emitted from more than one place, or from \
         nowhere.\n\
         Two detectors on one primitive drift, then disagree, and then the reader has \
         to work out which to believe — the same shape that cost this campaign a lane \
         on `report_layout_alias`. Forward to \
         `cratonvm_native_api::read_alias::observe_read` instead of re-implementing.\n\
         See W7-69-read-side-alias-instrument.md."
    );
}

// ---------------------------------------------------------------------------
// Link 2 — no new flag
// ---------------------------------------------------------------------------

/// The read-side census reuses `CRATONVM_DBG_LAYOUT_ALIAS` through
/// `layout_alias::enabled()` and names no environment variable of its own.
///
/// Two reasons, and the second is the mechanical one. The censuses are two
/// halves of one species, so a reader who turns on "the slot census" should get
/// both. And a new `CRATONVM_*` name has to be declared in four files
/// (`types/src/flag_groups.rs`, `types/tests/flag-surface.txt`,
/// `docs/flag-tokens.md`, `docs/config/flag-inventory.md`) or
/// `cargo test -p cratonvm-types` goes red — so a flag added here fails a test
/// in a crate the author is not looking at.
#[test]
fn the_read_side_census_adds_no_flag_of_its_own() {
    let src = strip_comments(&read("native-api/src/read_alias.rs"));
    assert!(
        !src.contains("CRATONVM_"),
        "`native-api/src/read_alias.rs` now names a CRATONVM_* environment variable \
         in code.\n\
         It must gate on `layout_alias::enabled()` instead. A new flag needs four \
         files (types/src/flag_groups.rs, types/tests/flag-surface.txt, \
         docs/flag-tokens.md, docs/config/flag-inventory.md) or \
         `cargo test -p cratonvm-types` goes red."
    );
    assert!(
        src.contains("layout_alias::enabled()"),
        "`read_alias` no longer gates on `layout_alias::enabled()`.\n\
         Without that gate the read-side observation runs on every field read in \
         every mode, which is a cost in Compatible mode — contractually frozen — for \
         a diagnostic almost every run has switched off."
    );
}

// ---------------------------------------------------------------------------
// Link 3 — the oracle walks the chain
// ---------------------------------------------------------------------------

/// `field_name_at` must consult superclasses.
///
/// `NativeContext::declared_fields` returns only the fields a class declares
/// **itself**, while `FieldMetadata::slot_index` is absolute. So an oracle that
/// looks at the receiver's own class and stops answers `Absent` for every
/// inherited field. On the calibration case that turns the finding from
/// "slot 0 is `java.nio.Buffer.mark`, not `hb`" into "slot 0 does not exist on
/// `java.nio.DirectByteBuffer`" — which is quieter, wrong, and would send the
/// next reader looking for an allocation-width bug that is not there.
///
/// `MockNativeContext::superclass_of` returns `None` unconditionally, so no
/// mock-based unit test can catch this. That is why it is a source gate.
#[test]
fn the_slot_name_oracle_walks_the_superclass_chain() {
    let src = strip_comments(&read("native-api/src/read_alias.rs"));
    let body = fn_body(&src, "field_name_at")
        .expect("read_alias.rs no longer defines `fn field_name_at` — the oracle moved");
    assert!(
        body.contains("super_of("),
        "`field_name_at` no longer walks `super_of`.\n\
         `declared_fields` is declared-only and `slot_index` is absolute, so without \
         the walk every INHERITED field reports as `Absent` — including \
         `java.nio.Buffer.mark`, which is the whole calibration case. The instrument \
         would go quiet on exactly the defect it was built for.\n\
         See W7-69-read-side-alias-instrument.md."
    );
    assert!(
        body.contains("declared_at("),
        "`field_name_at` no longer reads `declared_at` — it has no source of field \
         names left, so every answer is Absent or Unknown."
    );
}

// ---------------------------------------------------------------------------
// Link 4 — observation only
// ---------------------------------------------------------------------------

/// Every `read_alias::observe_read(` call in a native crate sits inside an
/// `if layout_alias::enabled() {` block that has **no `else`**.
///
/// The `else` half is the one that matters. `observe_read` returns an
/// `Option<ReadFinding>`, which exists so a test can tell "clean" from "not
/// looking" — and is exactly the shape someone reaches for when they decide the
/// diagnostic should also *fix* the read. It must not. A read that answers
/// differently with the flag on is a behaviour change in Compatible mode, which
/// is contractually frozen, and it also makes the census a measurement of
/// itself.
#[test]
fn every_read_side_observation_is_gated_and_observation_only() {
    let root = workspace_root();
    let mut offenders: Vec<String> = Vec::new();
    for crate_dir in NATIVE_CRATES {
        for file in rust_sources(&root.join(crate_dir).join("src")) {
            let raw = fs::read_to_string(&file).unwrap_or_default();
            let src = strip_comments(&raw);
            let name = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .display()
                .to_string()
                .replace('\\', "/");
            for (idx, _) in src.match_indices("read_alias::observe_read(") {
                // The nearest enclosing `if layout_alias::enabled() {` must be
                // within a short window above — it is written immediately above
                // the call at every site.
                let window_start = idx.saturating_sub(400);
                let window = &src[window_start..idx];
                // The gate may carry any path prefix: `if layout_alias::enabled() {`
                // and `if cratonvm_native_api::layout_alias::enabled() {` are the
                // same gate and both occur in the tree. Matching only the
                // unqualified spelling made this fire on two CORRECTLY gated sites
                // in `phases_early.rs` — which is the failure mode the sibling gate
                // in this file warns about in as many words: a gate that fires on
                // an untouched tree gets deleted, not investigated.
                let Some(gate_rel) = find_enabled_gate(window) else {
                    offenders.push(format!(
                        "{name}: an `observe_read` call with no `if layout_alias::enabled() {{` \
                         above it"
                    ));
                    continue;
                };
                let gate = window_start + gate_rel;
                let open = src[gate..].find('{').expect("matched above") + gate;
                let Some(close) = match_brace(&src, open) else {
                    offenders.push(format!("{name}: the enabled() gate's block never closes"));
                    continue;
                };
                if src[close + 1..].trim_start().starts_with("else") {
                    offenders.push(format!(
                        "{name}: the `if layout_alias::enabled()` block around an \
                         `observe_read` has an `else`"
                    ));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a read-side observation is not observation-only:\n  {}\n\
         The block must be `if layout_alias::enabled() {{ read_alias::observe_read(..); }}` \
         with NO else. With an else the read answers differently depending on a debug \
         flag, which is a behaviour change in Compatible mode (contractually frozen) \
         and makes the census a measurement of itself.\n\
         See W7-69-read-side-alias-instrument.md.",
        offenders.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// Link 5 — the calibration site stays observed, and stays observed FIRST
// ---------------------------------------------------------------------------

/// `native-io`'s `bb_resolve_heap_array` is the historical defect's own
/// function: its slot-0 fallback is the read that got `Buffer.mark = -1` and
/// called it `hb`. The observation must still be there, and must still run
/// **before** the `get_field` it is about.
///
/// The ordering half is not decoration. `bb_resolve_heap_array` returns early
/// on the two arms above the slot-0 fallback, so an observation moved below the
/// read — or below the `match` — is never reached on the receivers that
/// resolve, and is reached on the ones that do not only after the value has
/// already been taken. The same failure mode as
/// `layout_alias_coverage.rs`'s "observe before the clamp".
#[test]
fn the_calibration_site_is_still_observed_before_its_own_read() {
    let src = strip_comments(&read("native-io/src/lib.rs"));
    let body = fn_body(&src, "bb_resolve_heap_array").expect(
        "native-io/src/lib.rs no longer defines `fn bb_resolve_heap_array` — the \
         calibration site moved; carry the observation with it",
    );
    // Find the observation that is about SLOT 0 SPECIFICALLY. `find`/`rfind`
    // on the bare call is not enough and this was caught by simulating the
    // gate red: `bb_resolve_heap_array` holds a SECOND observation (the
    // deliberate non-firing control on `BB_SEGMENT_SLOT`), so deleting the
    // calibration one left both `find` and `rfind` satisfied by the survivor
    // and the gate stayed green against the exact mutation it exists for.
    let observe_at = body
        .match_indices("read_alias::observe_read(")
        .find(|(idx, _)| {
            let call: String = body[*idx..].chars().take(320).collect();
            call.contains("BB_FIELD_ARRAY") && call.contains("\"hb\"")
        })
        .map(|(idx, _)| idx)
        .unwrap_or_else(|| {
            panic!(
                "`bb_resolve_heap_array` no longer observes the read-side alias.\n\
             That call is the instrument's calibration point: slot 0 of a real \
             java.nio.DirectByteBuffer is `java.nio.Buffer.mark`, not the backing \
             array `hb` (which is 6). The read is IN BOUNDS, so nothing else in this \
             tree can see it — not the allocation-width census (it compares counts, \
             and no allocation happens here) and not the gc::guard out-of-bounds \
             list.\n\
             It is observation-only and costs one OnceLock load when the flag is off; \
             if it is in the way, move it, do not drop it.\n\
             See W7-69-read-side-alias-instrument.md."
            )
        });
    let final_read = body
        .rfind("get_field(this, BB_FIELD_ARRAY)")
        .expect("`bb_resolve_heap_array` no longer reads BB_FIELD_ARRAY — re-check this gate");
    assert!(
        observe_at < final_read,
        "the slot-0 observation in `bb_resolve_heap_array` now runs AFTER the read it \
         is about.\n\
         The two arms above it return early, so an observation below the read is \
         reached only on the receivers that already failed — and by then the wrong \
         value has been taken. Observe first, then read."
    );
}

/// Byte offset of an `if <path>layout_alias::enabled() {` gate in `window`,
/// searching from the end, or `None` when the window holds no gate.
///
/// Any path prefix counts. What the gate has to BE is `if` + a call to
/// `layout_alias::enabled()` + a block; spelling the module path out is legal
/// Rust and three sites in the tree do it. Only the `if` is load-bearing —
/// a bare `layout_alias::enabled()` in an expression is not a gate.
fn find_enabled_gate(window: &str) -> Option<usize> {
    const NEEDLE: &str = "layout_alias::enabled() {";
    let mut from = window.len();
    while let Some(rel) = window[..from].rfind(NEEDLE) {
        // Walk back over the path prefix (`a::b::`) to its first character.
        let mut start = rel;
        while start > 0 {
            let c = window.as_bytes()[start - 1];
            if c.is_ascii_alphanumeric() || c == b'_' || c == b':' {
                start -= 1;
            } else {
                break;
            }
        }
        if window[..start].trim_end().ends_with("if") {
            return Some(start);
        }
        if rel == 0 {
            break;
        }
        from = rel;
    }
    None
}

/// Every slot-map identifier `declare_slot_map` is called with, anywhere, with
/// any path qualification stripped off.
fn published_slot_map_idents(src: &str) -> std::collections::HashSet<String> {
    const CALL: &str = "declare_slot_map(&";
    let mut out = std::collections::HashSet::new();
    for (idx, _) in src.match_indices(CALL) {
        let arg: String = src[idx + CALL.len()..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
            .collect();
        if let Some(last) = arg.rsplit("::").next().filter(|s| !s.is_empty()) {
            out.insert(last.to_string());
        }
    }
    out
}

/// The two matchers above were widened to accept path-qualified spellings.
/// Widening a gate is how a gate stops catching anything, so both directions are
/// pinned here: what it must accept, and what it must still reject.
#[test]
fn the_gate_matchers_accept_paths_without_going_blind() {
    // --- find_enabled_gate: accepts ---
    assert!(
        find_enabled_gate("if layout_alias::enabled() {").is_some(),
        "the unqualified spelling is the common one"
    );
    assert!(
        find_enabled_gate("if cratonvm_native_api::layout_alias::enabled() {").is_some(),
        "the fully qualified spelling is what phases_early.rs uses"
    );
    assert!(
        find_enabled_gate("    if crate::layout_alias::enabled() {\n        foo();\n").is_some(),
        "a crate-relative path is still a gate"
    );

    // --- find_enabled_gate: still rejects ---
    assert!(
        find_enabled_gate("let on = layout_alias::enabled();").is_none(),
        "a bare call with no `if` is not a gate — this is the case the whole \
         instrument exists to catch"
    );
    assert!(
        find_enabled_gate("while layout_alias::enabled() {").is_none(),
        "only `if` is the gate shape the rationale is written about"
    );
    assert!(
        find_enabled_gate("if some_other::enabled() {").is_none(),
        "a different predicate is not this gate"
    );
    assert!(
        find_enabled_gate("").is_none(),
        "an empty window has no gate"
    );

    // --- published_slot_map_idents: accepts both spellings ---
    let found = published_slot_map_idents(
        "declare_slot_map(&MONTH_SLOT_MAP);\n\
         read_alias::declare_slot_map(&crate::lang_class::METHOD_LEGACY_SLOT_MAP);",
    );
    assert!(found.contains("MONTH_SLOT_MAP"));
    assert!(
        found.contains("METHOD_LEGACY_SLOT_MAP"),
        "a path-qualified argument publishes the map it names"
    );

    // --- published_slot_map_idents: still reports nothing for an unpublished map ---
    let none = published_slot_map_idents("static ORPHAN_SLOT_MAP: SlotMap = SlotMap::new();");
    assert!(
        !none.contains("ORPHAN_SLOT_MAP"),
        "declaring a map is not publishing it — if this ever passes, the orphan \
         check below can never fail"
    );
}

// ---------------------------------------------------------------------------
// Link 6 — a declared slot map is a published slot map
// ---------------------------------------------------------------------------

/// Every `SlotMap` value in the workspace is handed to `declare_slot_map`
/// somewhere.
///
/// A `SlotMap` nobody publishes is dead data: `verify_declared_slot_maps` sweeps
/// an empty list and reports zero rows, which reads as clean. That is the
/// vacuous-green shape — a probe that cannot fail — and it is worth a gate
/// because a slot map is exactly the kind of `const` a refactor unwires without
/// deleting.
#[test]
fn every_declared_slot_map_is_published() {
    let root = workspace_root();
    let mut declared: Vec<(String, String)> = Vec::new();
    let mut published = String::new();
    for crate_dir in ["native-api"].iter().chain(NATIVE_CRATES.iter()) {
        for file in rust_sources(&root.join(crate_dir).join("src")) {
            let src = strip_comments(&fs::read_to_string(&file).unwrap_or_default());
            let name = file
                .strip_prefix(&root)
                .unwrap_or(&file)
                .display()
                .to_string()
                .replace('\\', "/");
            published.push_str(&src);
            // `static NAME: ...SlotMap = ...` / `const NAME: ...SlotMap = ...`
            for keyword in ["static ", "const "] {
                for (idx, _) in src.match_indices(keyword) {
                    let rest = &src[idx + keyword.len()..];
                    let ident: String = rest
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if ident.is_empty() {
                        continue;
                    }
                    let after = &rest[ident.len()..];
                    let head: String = after.chars().take(160).collect();
                    // The declared TYPE must BE a `SlotMap`, not merely mention
                    // one. Caught by simulating this gate: `read_alias.rs`'s own
                    // `static MAPS: OnceLock<Mutex<Vec<&'static SlotMap>>>` — the
                    // registry the sweep reads — matched a "contains SlotMap"
                    // predicate and made the gate RED on an untouched tree. A
                    // gate that fires on the unmutated tree gets deleted, not
                    // investigated.
                    if !head.starts_with(':') {
                        continue;
                    }
                    let Some(eq) = head.find('=') else { continue };
                    let ty = head[1..eq].trim();
                    if ty.rsplit("::").next().map(str::trim) == Some("SlotMap") {
                        declared.push((name.clone(), ident));
                    }
                }
            }
        }
    }
    // A publication may name its map through any path:
    // `declare_slot_map(&MONTH_SLOT_MAP)` and
    // `declare_slot_map(&crate::lang_class::METHOD_LEGACY_SLOT_MAP)` both publish.
    // Matching the bare identifier reported the latter as an orphan while
    // `lang_reflect.rs` had been publishing it on every boot.
    let published_idents = published_slot_map_idents(&published);
    let orphans: Vec<String> = declared
        .iter()
        .filter(|(_, ident)| !published_idents.contains(ident.as_str()))
        .map(|(file, ident)| format!("{file}: {ident}"))
        .collect();
    assert!(
        orphans.is_empty(),
        "a SlotMap is declared but never published to the read-side sweep:\n  {}\n\
         `verify_declared_slot_maps` iterates only what `declare_slot_map` was given, \
         so an unpublished map means the sweep reports zero rows for that class — \
         which reads as clean when it is `not looking`. Call \
         `read_alias::declare_slot_map(&{{NAME}})` from the crate's registrar.\n\
         See W7-69-read-side-alias-instrument.md.",
        orphans.join("\n  ")
    );
    assert!(
        !declared.is_empty(),
        "no SlotMap is declared anywhere — the scanner broke, not the tree \
         (native-io's BB_SLOT_MAP should be found)"
    );
}

// ---------------------------------------------------------------------------
// Link 7 — the sweep is actually invoked
// ---------------------------------------------------------------------------

/// `verify_declared_slot_maps` has at least one caller outside the module that
/// defines it.
///
/// Link 6 proves a `SlotMap` reaches the registry. It proves nothing about
/// anything ever *reading* the registry, and for the whole of 2026-08-12
/// nothing did: three lanes (W7-75, W7-76, W7-77) each published a map and each
/// closed with the same residual — "the sweep has no caller; choosing its
/// trigger needs a build". Seven maps and 29 declared slots were pointed at a
/// function nobody called.
///
/// **A detector with no caller is indistinguishable from a detector reporting
/// all-clear.** That is this campaign's dominant species, and it had taken up
/// residence inside the instrument built to detect it.
///
/// `native-api/src` is excluded from the scan on purpose: `read_alias.rs`
/// contains the definition, its own doc links and the `sweep_declared_slot_maps_at`
/// wrapper's internal call, so a predicate that counted this crate would be
/// green on a tree where nothing outside it calls anything — the exact vacuous
/// shape. Comments are stripped for the same reason link 6's predicate had to
/// be tightened: six `///` lines in the native crates name the sweep, and a
/// gate satisfied by prose is satisfied on a tree with no caller.
#[test]
fn the_declared_slot_map_sweep_has_a_caller() {
    let root = workspace_root();
    let mut callers: Vec<String> = Vec::new();
    for crate_dir in ["vm", "vm-cli", "gc", "jit"]
        .iter()
        .chain(NATIVE_CRATES.iter())
    {
        for file in rust_sources(&root.join(crate_dir).join("src")) {
            let src = strip_comments(&fs::read_to_string(&file).unwrap_or_default());
            if !src.contains("sweep_declared_slot_maps_at(")
                && !src.contains("verify_declared_slot_maps(")
            {
                continue;
            }
            callers.push(
                file.strip_prefix(&root)
                    .unwrap_or(&file)
                    .display()
                    .to_string()
                    .replace('\\', "/"),
            );
        }
    }
    // Printed, not asserted on: the SET of triggers is expected to grow (an
    // embedder teardown, `DestroyJavaVM`) and a gate on the exact list would be
    // re-baselined on sight. What must never be empty is the list itself.
    println!("declared-slot-map sweep callers: {callers:?}");
    assert!(
        !callers.is_empty(),
        "nothing outside `native-api/src` calls the declared-slot-map sweep.\n\
         Every `SlotMap` in the workspace is then published to a function that never \
         runs, and `verify_declared_slot_maps` reporting zero rows means `it was never \
         asked`, not `the slot maps agree with the loaded classes`. Those two readings \
         are opposite and the census cannot tell them apart.\n\
         Wire a trigger at a point where the classes are LOADED — the launcher's \
         post-`main` teardown and the `System.exit` natives are the two this tree \
         uses.\n\
         See W7-90-slot-map-sweep-caller.md."
    );
}

// ---------------------------------------------------------------------------
// Link 8 — the launcher's trigger runs after the workload
// ---------------------------------------------------------------------------

/// The post-`main` sweep sits **below** the `main(String[])` invoke in
/// `vm-cli`'s `run()`.
///
/// The ordering is the whole justification for the trigger's placement, not a
/// detail. W7-69 §2 rejected a registration-time check with a reason that is a
/// property of this tree: at registration most of the declared classes are not
/// loaded, `declared_fields` comes back empty, and `declared == 0` is the
/// documented overload for *unmeasured, not cleared* —
/// `java.nio.DirectByteBuffer` is package-private and is not among the 323
/// classes `bootstrap_core_classes` names. A sweep lifted above the invoke
/// silently becomes that rejected check while still printing a census, which is
/// worse than not running: it reports `unresolved` where the tree has defects.
#[test]
fn the_post_main_sweep_runs_after_the_workload() {
    let src = strip_comments(&read("vm-cli/src/main.rs"));
    let body = fn_body(&src, "run")
        .expect("vm-cli/src/main.rs no longer defines `fn run()` — the launcher moved");
    let sweep = body
        .find("vm.sweep_declared_slot_maps(")
        .unwrap_or_else(|| {
            panic!(
                "`vm-cli`'s `run()` no longer sweeps the declared slot maps after `main` \
             returns.\n\
             That call is the read-side census's primary trigger. Without it the seven \
             published `SlotMap`s are swept only on the `System.exit` paths, and a \
             workload that returns normally prints no census at all — which reads as \
             clean.\n\
             It is observation-only, gated on `layout_alias::enabled()` with no `else`, \
             and costs one OnceLock load and a branch once per process when the flag is \
             off. If it is in the way, move it, do not drop it.\n\
             See W7-90-slot-map-sweep-caller.md."
            )
        });
    let main_invoke = body
        .rfind("\"main\",")
        .expect("`run()` no longer invokes a method literally named `main` — re-check this gate");
    assert!(
        sweep > main_invoke,
        "the declared-slot-map sweep in `run()` now runs BEFORE the `main(String[])` \
         invoke.\n\
         Above the workload it is the registration-time check W7-69 §2 rejected: the \
         classes the maps name are mostly not loaded yet, `declared_fields` answers \
         empty, and `Unknown` (unmeasured) is indistinguishable from clean. It would \
         still print a census — of nothing.\n\
         See W7-90-slot-map-sweep-caller.md."
    );
}

// ---------------------------------------------------------------------------
// Link 9 — the self-terminating paths sweep too, and observe only
// ---------------------------------------------------------------------------

/// `System.exit`, `Runtime.exit` and `Runtime.halt` each sweep before they
/// terminate, and the helper they share is gated with no `else`.
///
/// Link 8's trigger is never reached when the application ends itself — and
/// that is how nearly every fixture this instrument is aimed at ends (SbRunner,
/// Surefire's `ForkedBooter`, every Spring Boot app). A sweep wired only to the
/// return path reports nothing on exactly the runs that matter, which is link
/// 7's failure wearing a different hat.
///
/// Each call is matched together with **its own trigger label**, not by the
/// bare function name. That is link 5's lesson applied: `bb_resolve_heap_array`
/// held a second `observe_read`, so deleting the calibration one left both
/// `find` and `rfind` satisfied and the gate stayed green against the exact
/// mutation it exists for. Three exit natives share one helper here, so a
/// name-only predicate would let two of the three be deleted.
#[test]
fn the_exit_paths_sweep_before_they_terminate() {
    let src = strip_comments(&read("native-builtins/src/lang_system.rs"));

    for (native, trigger) in [
        ("native_system_exit", "\"System.exit\""),
        ("native_runtime_exit", "\"Runtime.exit\""),
        ("native_shutdown_halt0", "\"Runtime.halt\""),
    ] {
        let body = fn_body(&src, native).unwrap_or_else(|| {
            panic!("native-builtins/src/lang_system.rs no longer defines `fn {native}`")
        });
        let sweep = body
            .match_indices("sweep_declared_slot_maps_before_exit(")
            .find(|(idx, _)| {
                let call: String = body[*idx..].chars().take(160).collect();
                call.contains(trigger)
            })
            .map(|(idx, _)| idx)
            .unwrap_or_else(|| {
                panic!(
                    "`{native}` no longer sweeps the declared slot maps with the {trigger} \
                     label before terminating.\n\
                     The launcher's post-`main` trigger is unreachable on this path, so \
                     dropping it means a workload that exits itself prints no read-side \
                     census — and no census reads as clean.\n\
                     See W7-90-slot-map-sweep-caller.md."
                )
            });
        let terminate = body.rfind("std::process::exit(").unwrap_or_else(|| {
            panic!("`{native}` no longer calls `std::process::exit` — re-check this gate")
        });
        assert!(
            sweep < terminate,
            "`{native}` sweeps AFTER `std::process::exit`, which never returns — the \
             call is dead code that looks like coverage."
        );
    }

    // The shared helper is gated, and the block has no `else`. Same rule as
    // link 4 and for the same reason: a diagnostic that changes behaviour is a
    // behaviour change in Compatible mode, which is contractually frozen.
    let helper = fn_body(&src, "sweep_declared_slot_maps_before_exit")
        .expect("lang_system.rs no longer defines the exit-path sweep helper");
    let gate = helper
        .find("layout_alias::enabled() {")
        .expect("the exit-path sweep helper no longer gates on `layout_alias::enabled()`");
    let open = helper[gate..].find('{').expect("matched above") + gate;
    let close = match_brace(helper, open).expect("the helper's gate block never closes");
    assert!(
        !helper[close + 1..].trim_start().starts_with("else"),
        "the exit-path sweep helper's `if layout_alias::enabled()` block has an \
         `else`.\n\
         The sweep is observation-only. With an `else` the exit path behaves \
         differently depending on a debug flag, which is a behaviour change in \
         Compatible mode — contractually frozen."
    );
}

// ---------------------------------------------------------------------------
// Link 10 — the Surefire fork's own exit paths
// ---------------------------------------------------------------------------

/// LINK 10. `System.exit` is not the only way a fixture leaves. Four triples on
/// `ForkedBooter` are registered from `register_essential_natives_with_shims`
/// — LIVE in both modes — onto three bodies that each end in their own
/// `std::process::exit` and never reach `native_system_exit`. A Surefire fork
/// therefore skips the launcher's post-`main` line AND link 9's three natives,
/// which is the population W7-90 §2.2 named as its whole reason for existing.
///
/// Each call is matched WITH its own trigger label, not by the helper's name:
/// three bodies share one helper, so a name-only predicate stays green when two
/// of the three are deleted (§5.1).
#[test]
fn the_surefire_exit_paths_sweep_before_they_terminate() {
    let src = strip_comments(&read("native-builtins/src/test_frameworks.rs"));
    // The label is matched WITH its quotes, exactly as link 9 does: an
    // unquoted "ForkedBooter.exit" is a prefix of "ForkedBooter.exit1".
    for (body, label) in [
        (
            "native_surefire_forkedbooter_acknowledged_exit",
            "\"ForkedBooter.acknowledgedExit\"",
        ),
        (
            "native_surefire_forkedbooter_exit1",
            "\"ForkedBooter.exit1\"",
        ),
        (
            "native_surefire_forkedbooter_exit_code",
            "\"ForkedBooter.exit\"",
        ),
    ] {
        let b =
            fn_body(&src, body).unwrap_or_else(|| panic!("{body} not found in test_frameworks.rs"));
        let sweep = b
            .find("sweep_declared_slot_maps_before_exit(")
            .unwrap_or_else(|| {
                panic!(
                    "{body} terminates the process with std::process::exit and never \
                 sweeps. A Surefire fork skips the launcher trigger AND the three \
                 lang_system natives, so the declared slot-map sweep prints nothing \
                 on exactly the fixtures W7-90 section 2.2 was written for."
                )
            });
        assert!(
            b[sweep..].contains(label),
            "{body}'s sweep call must carry its own trigger label {label:?}; three \
             bodies share one helper and a name-only match stays green when two of \
             the three calls are deleted"
        );
        let exit = b.find("std::process::exit").unwrap_or_else(|| {
            panic!("{body} no longer exits — re-derive this gate before deleting it")
        });
        assert!(sweep < exit, "{body} sweeps AFTER it has already exited");
    }
}

// ---------------------------------------------------------------------------
// The census — printed, not asserted
// ---------------------------------------------------------------------------

/// Print the read-side population: how many constant-slot field accesses each
/// native crate makes, and how many of them state an expected field name.
///
/// Deliberately not a ratchet. The gap between the two columns is the honest
/// remainder — the reads whose intent lives only in a `const F_x: usize = k`
/// name and a comment, which no instrument can check. Printing it is the point:
/// a silent remainder gets read as coverage.
#[test]
fn census() {
    let root = workspace_root();
    let mut total = 0usize;
    let mut observed = 0usize;
    let mut per_crate: Vec<(String, usize, usize)> = Vec::new();
    for crate_dir in NATIVE_CRATES {
        let (mut c_total, mut c_obs) = (0usize, 0usize);
        for file in rust_sources(&root.join(crate_dir).join("src")) {
            let src = strip_comments(&fs::read_to_string(&file).unwrap_or_default());
            for accessor in [
                "get_field(",
                "set_field(",
                "get_field_typed(",
                "get_field_volatile(",
                "set_field_volatile(",
            ] {
                for (idx, _) in src.match_indices(accessor) {
                    let prev = src[..idx].chars().next_back().unwrap_or(' ');
                    if prev != '.' {
                        continue;
                    }
                    // Second argument a decimal literal or an UPPER_SNAKE const:
                    // that is a slot map asserting itself, as opposed to an index
                    // resolved by name at run time.
                    let args = &src[idx + accessor.len()..];
                    let Some(comma) = args.find(',') else {
                        continue;
                    };
                    let second: String = args[comma + 1..]
                        .trim_start()
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if second.is_empty() {
                        continue;
                    }
                    let constant = second.chars().all(|c| c.is_ascii_digit())
                        || second
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                    if constant {
                        c_total += 1;
                    }
                }
            }
            c_obs += src.matches("read_alias::observe_read(").count();
        }
        total += c_total;
        observed += c_obs;
        if c_total + c_obs > 0 {
            per_crate.push(((*crate_dir).to_string(), c_total, c_obs));
        }
    }
    println!("read-side alias census — constant-slot field accesses in native crates");
    for (name, t, o) in &per_crate {
        println!("  {name:28} constant-slot {t:6}   observed {o:4}");
    }
    println!(
        "  {:28} constant-slot {total:6}   observed {observed:4}",
        "TOTAL"
    );
    println!(
        "  the gap is the honest remainder: reads whose expected field name exists only \
         in a `const F_x: usize = k` identifier and a comment. See \
         W7-69-read-side-alias-instrument.md for the first census over the subset that \
         names its class."
    );
    assert!(
        total > 0,
        "no constant-slot field accesses found at all — the scanner broke, not the tree"
    );
}
