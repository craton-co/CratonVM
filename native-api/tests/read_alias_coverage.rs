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
//!
//! Plus [`census`], which **prints** the read-side population and asserts only
//! that it is non-zero. Deliberately not a ratchet, for the same reason
//! `layout_alias_coverage.rs`'s census is not: a count over a population that
//! changes with every native added gets re-baselined on sight, and a gate people
//! re-baseline teaches them to re-baseline the whole file.
//!
//! # What this gate deliberately does NOT assert
//!
//! It does not require every literal-slot read in the workspace to carry an
//! expected field name. There are ~7,200 of them and most read objects the
//! native allocated itself, where the slot map is the class's own truth. A gate
//! that failed on all of them would be muted on the day it landed. The
//! remainder is **printed** by [`census`] instead, so it is a visible number
//! rather than a silent one.

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
fn fn_body<'a>(src: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("fn {name}(");
    let start = src.find(&needle)?;
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
            if src.contains("direction = \"wrong-field\"") || src.contains("direction = \"absent-slot\"")
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
                let Some(gate_rel) = window.rfind("if layout_alias::enabled() {") else {
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
    let orphans: Vec<String> = declared
        .iter()
        .filter(|(_, ident)| !published.contains(&format!("declare_slot_map(&{ident})")))
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
                    let Some(comma) = args.find(',') else { continue };
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
    println!("  {:28} constant-slot {total:6}   observed {observed:4}", "TOTAL");
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
