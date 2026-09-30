// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! TYPED-ARRAY-PRODUCER RATCHET — a two-sided, per-file count of the untyped
//! reference-array spellings in `cratonvm-native-builtins`.
//!
//! ## Why
//!
//! A native whose descriptor returns a narrow `T[]` (`String[]`,
//! `Provider[]`, `Class[]`, ...) must allocate that array with the declared
//! component. The untyped spellings — `new_array(ArrayElementType::Reference,
//! ..)` and `new_ref_array(ClassId::new(0), ..)` — build an `Object[]`
//! instead. Under the lenient array-cast rule nobody notices; under the strict
//! rule (the `--jdk-only` default since interpreter round i1 wave 7) the first
//! `(T[])` of that result, or of its `clone()`, is a `ClassCastException`
//! far from the native. Waves 6-8 typed every producer a survey found
//! (`docs/internal/fixed-bugs/interpreter-L2-object-array-producers-block-strict-array-cast-FIXED-20260924.md`,
//! `docs/internal/fixed-bugs/interpreter-L2-synthetic-jdk-overrides-return-untyped-arrays-FIXED-20260924.md`),
//! and in wave 7 and 8 most of them turned out to hold a raw reference across
//! an allocation as well: the typed builders
//! (`t27_tls::materialize_java_string_array`, `lang_class::retyped_ref_array`,
//! `NativeHandleScope`) fix both.
//!
//! Nothing stopped the next native from writing the easy spelling again. This
//! gate does: it is stage 1 of
//! `docs/internal/fixed-bugs/interpreter-L2-proposal-typed-array-producer-ratchet-FIXED-20261003.md`.
//!
//! ## What it counts
//!
//! Per file under `src/`, after dropping `//` comment lines and ALL
//! whitespace (so a call split over several lines by rustfmt still matches),
//! the occurrences of the four [`NEEDLES`]. It over-approximates on purpose:
//! most hits are legitimate `Object[]` allocations (collection backing
//! stores, enumeration snapshots, natives declared to return `Object[]`),
//! and `#[cfg(test)]` code counts too. The point is that a NEW one fails CI
//! and makes its author look at the descriptor.
//!
//! ## Two-sided
//!
//! Every file's count must EQUAL its row in [`BASELINE`] (a file absent from
//! the table must count 0):
//!
//! * a count that ROSE means a new untyped producer. If the native returns a
//!   narrow `T[]`, type it (`lang_class::loading_component_id` with no raw
//!   reference live, `reflection_component_id` for a surely-loaded class, the
//!   builders above for `String[]` / a copy). If it is a genuine `Object[]`
//!   (a backing store, an `Object[]` return), raise that file's row in the
//!   same change and say why in the review.
//! * a count that FELL is a win: lower the row in the same change, so it
//!   cannot silently leak back.
//!
//! The failure message prints the whole current table, ready to paste.
//!
//! A reproduction of the count without cargo, for one file:
//!
//! ```text
//! grep -v '^[[:space:]]*//' src/FILE.rs | tr -d ' \t\r\n\f\v' \
//!   | grep -o 'new_array(ArrayElementType::Reference\|new_array(cratonvm_types::ArrayElementType::Reference\|new_ref_array(ClassId::new(0)\|new_ref_array(cratonvm_types::ClassId::new(0)' \
//!   | wc -l
//! ```
//!
//! ```text
//! cargo test -p cratonvm-native-builtins --test typed_array_producer_ratchet -- --nocapture
//! ```

use std::path::{Path, PathBuf};

/// The untyped reference-array spellings, whitespace already removed.
/// Disjoint: no needle is a substring of another, so a site counts once.
const NEEDLES: [&str; 4] = [
    "new_array(ArrayElementType::Reference",
    "new_array(cratonvm_types::ArrayElementType::Reference",
    "new_ref_array(ClassId::new(0)",
    "new_ref_array(cratonvm_types::ClassId::new(0)",
];

/// Frozen per-file counts, paths relative to the crate root with `/`.
///
/// Measured 2026-09-24 (interpreter round i1 wave 8, lane L2) on the wave-8
/// tree, after that lane typed the `synthetic-jdk` override producers. Zero
/// slack in either direction.
///
/// Re-measured 2026-09-26 on the gc-common round (w34) after it merged
/// `origin/dev` `f144bb3d2`: ten rows fell (handle-scope rewrites that dropped
/// or merged producers) and five rose, every rise a `#[cfg(test)]` mock
/// fixture, not a native. See
/// `docs/internal/gc-common-round-20260923/w34-ratchets-report.md`.
const BASELINE: &[(&str, usize)] = &[
    ("src/antlr_intrinsics.rs", 5),
    ("src/apps_h2.rs", 2),
    ("src/cglib_enhancer.rs", 1),
    ("src/classloader.rs", 13),
    ("src/deprecated_io_util.rs", 1),
    ("src/deprecated_util.rs", 1),
    ("src/http_client.rs", 3),
    ("src/http_url_connection.rs", 2),
    ("src/jboss_module_loader.rs", 4),
    ("src/jca/provider_chain.rs", 2),
    ("src/jfr.rs", 1),
    ("src/jmx.rs", 16),
    ("src/jmx_openmbean.rs", 6),
    ("src/lang_class.rs", 15), // gc-common 14->15 for the w29e test fixture (a mock Object[])
    ("src/lang_invoke.rs", 26), // interp i1 w45 L4: -2 (`build_method_type_from_descriptor` and the constructor MemberName type build `MethodType.ptypes` as `Class[]`), merged over: JIT round 14 w7 lane ffm7 +3: the IndirectVarHandle `invokeWithArguments` argument array, the `methodHandleTable` fallback when `MethodHandle` is unresolved, and one test fixture table, all genuine Object[]; JIT round 11 w14 lane vh: four test fixtures, genuine Object[] receivers; JIT round 12 w5 lane hunter3 +8: `r12w5_invoke_declared_cast_tests` Object[] argument arrays for invokeWithArguments (test-only); interp i1 w26 L4: the spread-invoker test's two spread arrays (net +1); w27 L4: two reference-cast test fixtures; all genuine Object[]
    ("src/lang_math.rs", 1),
    ("src/lang_misc.rs", 3),
    ("src/lang_stackwalker.rs", 1), // gc-common w19-g test fixture: a mock frames buffer
    ("src/lang_string.rs", 2),
    ("src/lang_system.rs", 3),
    ("src/lib.rs", 14),
    ("src/locale_resources.rs", 2),
    ("src/logmanager.rs", 5),
    ("src/lookup_define.rs", 3),
    ("src/lucene_es.rs", 3),
    ("src/mapper_memo_apply_tests.rs", 1), // gc-common w34-b test-only mock: `ref_array` fixture
    ("src/net_phase_e.rs", 10),
    ("src/panama.rs", 23), // round 12 w7 (upcall): the old upcall callback's `Object[]` went with it
    ("src/panama_libffi.rs", 3),
    ("src/phases_early.rs", 32),
    ("src/phases_late.rs", 9),
    ("src/phases_late/bouncycastle.rs", 4),
    ("src/phases_late/charset_buffers.rs", 1),
    ("src/phases_late/collections.rs", 9),
    ("src/phases_late/concurrent.rs", 21),
    ("src/phases_late/foreign_ffm.rs", 19), // round 12 w7 (ffm4): the `reinterpret` cleanup record `Object[2]` + two test fixtures
    ("src/phases_late/io_streams.rs", 2),
    ("src/phases_late/jar_manifest.rs", 5),
    ("src/phases_late/jdbc.rs", 1),
    ("src/phases_late/net_channels.rs", 1),
    ("src/phases_late/nio_file.rs", 22), // gc-common: +3 w20-d test fixtures, -1 network-interface producers merged
    ("src/phases_late/reflect_invoke.rs", 4),
    ("src/phases_late/ssl_security.rs", 1),
    ("src/phases_late/streams.rs", 26), // gc-common 25->26 for the w17-f test fixture (a mock backing store)
    ("src/phases_late/text_intl.rs", 3),
    ("src/phases_late/xml_json.rs", 10),
    ("src/phases_late/zip_streams.rs", 4),
    ("src/properties_sidetable.rs", 1),
    ("src/proxy_selector.rs", 1),
    ("src/reflect_annotations.rs", 1),
    ("src/service_loader.rs", 10),
    ("src/servlet.rs", 3),
    ("src/streams.rs", 2),
    ("src/t27_tls.rs", 1),
    ("src/t3_impl.rs", 11),
    ("src/test_frameworks.rs", 2),
    ("src/unsafe_natives.rs", 1),
    ("src/util_concurrent_ext.rs", 14), // incl. dev w13-f test fixture: a genuine Object[]
    ("src/wildfly_core.rs", 1),
    ("src/wildfly_security.rs", 1),
    ("src/wildfly_undertow.rs", 1),
    ("src/xml_xerces.rs", 2),
    ("src/xnio_async.rs", 1),
    ("src/xnio_conduits.rs", 1),
    ("src/xnio_worker.rs", 1),
];

/// A scan that stops finding its own source reports zero everywhere, which
/// a per-file equality would flag file by file — but a floor names the real
/// cause (the walk broke) in one line.
const MIN_LINES_SCANNED: usize = 100_000;

/// Belt-and-braces against needles that stop matching (a changed spelling in
/// the scan itself): the measured total was 423.
const MIN_TOTAL: usize = 200;

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `vendor/` holds third-party sources this crate does not own.
            if path.file_name().is_some_and(|n| n == "vendor") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The untyped-producer count of one file's source, and its line count.
///
/// `str::lines` strips a trailing `\r`, and every whitespace character is
/// dropped anyway, so an LF and a CRLF checkout count the same.
fn count_untyped(src: &str) -> (usize, usize) {
    let mut squeezed = String::with_capacity(src.len());
    let mut lines = 0usize;
    for line in src.lines() {
        lines += 1;
        if line.trim_start().starts_with("//") {
            continue;
        }
        squeezed.extend(line.chars().filter(|c| !c.is_whitespace()));
    }
    let hits = NEEDLES.iter().map(|n| squeezed.matches(n).count()).sum();
    (hits, lines)
}

/// `(path, count)` for every file with a nonzero count, sorted by path, and
/// the number of lines scanned.
fn census() -> (Vec<(String, usize)>, usize) {
    let root = crate_root();
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    let mut rows = Vec::new();
    let mut lines_scanned = 0usize;
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let src = String::from_utf8_lossy(&bytes);
        let (hits, lines) = count_untyped(&src);
        lines_scanned += lines;
        if hits > 0 {
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            rows.push((rel, hits));
        }
    }
    rows.sort();
    (rows, lines_scanned)
}

#[test]
fn the_needles_match_a_split_call_and_skip_comments() {
    let src =
        "let a = ctx.new_array(\n    cratonvm_types::ArrayElementType::Reference,\n    n,\n);\n\
               // ctx.new_ref_array(ClassId::new(0), 1)\n\
               let b = scope.new_ref_array(ClassId::new(0), 2);\n\
               let c = ctx.new_ref_array(component, 3);\n";
    assert_eq!(count_untyped(src), (2, 7));
}

#[test]
fn untyped_reference_array_producers_match_the_frozen_table() {
    let (rows, lines) = census();
    let total: usize = rows.iter().map(|(_, n)| n).sum();
    println!(
        "typed-array-producer ratchet: {total} untyped producers in {} files, {lines} lines",
        rows.len()
    );

    assert!(
        lines >= MIN_LINES_SCANNED,
        "only {lines} lines scanned (floor {MIN_LINES_SCANNED}): the walk broke, \
         not the crate — fix the scan before touching the table."
    );
    assert!(
        total >= MIN_TOTAL,
        "only {total} untyped producers found (floor {MIN_TOTAL}): the needles \
         stopped matching — fix the scan before touching the table."
    );

    let mut problems = Vec::new();
    for (path, count) in &rows {
        let frozen = BASELINE
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        if *count > frozen {
            problems.push(format!(
                "{path}: {count} untyped reference-array allocations, frozen at {frozen} — \
                 a new `new_array(ArrayElementType::Reference, ..)` / \
                 `new_ref_array(ClassId::new(0), ..)`. If its native returns a narrow `T[]`, \
                 allocate the declared component; if it is a genuine `Object[]`, raise the row."
            ));
        } else if *count < frozen {
            problems.push(format!(
                "{path}: {count} untyped reference-array allocations, frozen at {frozen} — \
                 good, now lower the row to {count} in this same change."
            ));
        }
    }
    for (path, frozen) in BASELINE {
        if !rows.iter().any(|(p, _)| p == path) {
            problems.push(format!(
                "{path}: 0 untyped reference-array allocations (or the file is gone), frozen at \
                 {frozen} — remove the row in this same change."
            ));
        }
    }

    if !problems.is_empty() {
        let table: Vec<String> = rows
            .iter()
            .map(|(p, n)| format!("    (\"{p}\", {n}),"))
            .collect();
        panic!(
            "{}\n\nThe current table, for BASELINE:\n{}",
            problems.join("\n"),
            table.join("\n")
        );
    }
}
