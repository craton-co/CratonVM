/// Every `key=` in the `[GC]` summary must be unique across the WHOLE
/// summary, not merely within its own line.
///
/// # Why this is a test and not a style note
///
/// The summary is an interface, and the only way anyone consumes it is
/// `grep`. A key that appears on two lines therefore has no answer: the
/// reader gets two matches for unrelated quantities and the obvious
/// `| tail -1` silently picks whichever comes last.
///
/// That is not hypothetical. Both of these cost real analysis time on
/// 2026-09-05:
///
/// * `objects_relocated=` was printed by whole-heap compaction AND by
///   `zgc-high-compaction`, whose large-object counts are tiny beside it.
///   An H2 measurement read **12** where the run had relocated **601233**.
/// * `cycles=` was printed by `par_evac` and `moving_young`. A GC control
///   in the same session matched `par_evac`'s — which is the GENERATIONAL
///   evacuator and reads zero under ZGC whatever ZGC did — and reported a
///   vacuous zero as if the heap had never collected.
///
/// Ten keys collided when this test was written. The fix is a per-line
/// prefix (`high_`, `unwind_`, `zr_`, `par_evac_`, `refill_`/`retry_`);
/// the rule is that a new summary field may not reuse a name another line
/// already owns.
#[test]
fn every_gc_summary_key_is_unique_across_the_whole_summary() {
    // `tlab.rs` and `evac_pool.rs` are in the corpus as well as this file.
    // `tlab_census_lines()` and `evac_pool_census_line()` FORMAT `[GC]`
    // lines there and `print_gc_summary` PRINTS them, so they are part of
    // the same summary and owe the same uniqueness — but scanning only
    // this file would never see them, and a key colliding across that seam
    // is precisely the failure this test exists for. Any future summary
    // line formatted outside `vm_heap.rs` belongs here too.
    //
    // Establish the corpus before concluding anything from it: a file that
    // stopped carrying the summary would make this pass vacuously.
    let mut lines: Vec<&str> = Vec::new();
    for src in [
        concat!(include_str!("../vm_heap/mod.rs"), "\n", include_str!("../vm_heap/compact_copy_plan.rs"), "\n", include_str!("../vm_heap/as_collector.rs"), "\n", include_str!("../vm_heap/soft_ref_policy_free_mb.rs"), "\n", include_str!("../vm_heap/render_tlab_guard_line.rs")),
        include_str!("../../../gc-core/src/tlab.rs"),
        include_str!("../../../gc-core/src/evac_pool.rs"),
    ] {
        for (i, _) in src.match_indices("\"[GC]") {
            let rest = &src[i + 1..];
            if let Some(end) = rest.find('"') {
                lines.push(&rest[..end]);
            }
        }
    }
    // gen r4w2/obs: `print_gc_summary` also prints
    // `GenerationalHeap::young_trigger_census_line()`, formatted in
    // `gen_heap.rs` (`YoungTriggerCensus::line`). Only THAT literal is
    // taken from `gen_heap.rs` — the file's other `"[GC]` strings are
    // per-collection lines (`--verbose:gc`), not the summary.
    // gen r4w4/alloc4: and `GenerationalHeap::alloc_path_census_line()`
    // (`[GC] gen-alloc:`), printed right after it.
    // gcd d9/b: and `young_trigger_wedged_census_line()` /
    // `oome_ladder_census_line()`.
    for prefix in [
        "\"[GC] young_trigger:",
        "\"[GC] gen-alloc:",
        "\"[GC] young_trigger_wedged:",
        "\"[GC] oome_ladder:",
        "\"[GC] pinned_copy_defrag:",
        "\"[GC] young_premium:",
        // gcj j1/yg: `term4_offer_census_line()`.
        "\"[GC] young_term4_offer:",
        // gcj yg3-residuals R3: `young_geometry_census_line()`.
        "\"[GC] young_geometry:",
    ] {
        let gen_src = concat!(include_str!("../../../gc-gen/src/gen_heap/mod.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/young_from_guard.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/geometry_cap_destination.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/publish_young_commit_bits.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/set_resolved_compact_primitive_field.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/set_field_full.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/non_moving_young_cycle.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/collect_garbage_inner_with_pins.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/young_drain_blocked.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/sweep_young_non_moving.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/sweep_old_gen_non_moving.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/mark_young_to_old_refs_deferring.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/cell_corrupt_diag_enabled.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/note_rejected_old_mark_candidate.rs"), "\n", include_str!("../../../gc-gen/src/gen_heap/zero_run_verdict.rs"));
        let starts: Vec<usize> = gen_src.match_indices(prefix).map(|(i, _)| i).collect();
        // (A unit test in gen_heap.rs asserts on the rendered prefix; that
        // literal carries no `key={` and adds nothing to the scan.)
        assert!(
            !starts.is_empty(),
            "no {prefix} format string in gen_heap.rs -- the corpus would \
             silently stop covering that summary line"
        );
        for i in starts {
            let rest = &gen_src[i + 1..];
            if let Some(end) = rest.find('"') {
                lines.push(&rest[..end]);
            }
        }
    }
    // gen r4w3/obs2: likewise `GcNotificationCensus::summary_line`, the
    // one `[GC]` summary line `gc_metrics.rs` formats for this function.
    {
        let metrics_src = include_str!("../../../gc-core/src/gc_metrics.rs");
        let starts: Vec<usize> = metrics_src
            .match_indices("\"[GC] gc_notifications:")
            .map(|(i, _)| i)
            .collect();
        assert!(
            !starts.is_empty(),
            "no `[GC] gc_notifications:` format string in gc_metrics.rs -- the \
             corpus would silently stop covering that summary line"
        );
        for i in starts {
            let rest = &metrics_src[i + 1..];
            if let Some(end) = rest.find('"') {
                lines.push(&rest[..end]);
            }
        }
    }
    assert!(
        lines.len() > 20,
        "expected the [GC] summary to have many lines, found {} -- this scan is reading the wrong text and its verdict means nothing",
        lines.len()
    );

    // `key={` is the format-string form; that is what a reader greps for.
    let keys_of = |line: &str| -> Vec<String> {
        let mut out = Vec::new();
        let b = line.as_bytes();
        for (i, _) in line.match_indices("={") {
            let mut s = i;
            while s > 0 {
                let c = b[s - 1];
                if c.is_ascii_alphanumeric() || c == b'_' {
                    s -= 1;
                } else {
                    break;
                }
            }
            if s < i {
                let k = &line[s..i];
                if !k.chars().next().unwrap_or('0').is_ascii_digit()
                    && !out.contains(&k.to_string())
                {
                    out.push(k.to_string());
                }
            }
        }
        out
    };

    let mut owner: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut collisions: Vec<String> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        for k in keys_of(line) {
            match owner.get(&k) {
                Some(&first) if first != idx => collisions.push(k),
                _ => {
                    owner.entry(k).or_insert(idx);
                }
            }
        }
    }
    collisions.sort();
    collisions.dedup();
    assert!(
        collisions.is_empty(),
        "these [GC] summary keys appear on more than one line, so grepping the summary for them returns unrelated quantities and `| tail -1` picks an arbitrary one: {collisions:?}. Give the newer line's field a prefix of its own."
    );
}

/// gc-common w3-f (2026-09-23): no `[GC]` literal in this file carries a
/// long run of spaces BETWEEN two words.
///
/// Eleven did. Each was a `"… \` + newline + indent` continuation whose
/// trailing backslash had been lost (a reflow joined the lines), so the
/// indentation became part of the string: `[GC] zgc-trigger: stress=3
/// live_bytes_threshold=…` followed by 22 spaces and the next key, on
/// every exit census — and the same in one `tracing::warn!` message and
/// five assertion messages, which this test does not scan. `split_whitespace` scrapers never noticed,
/// which is why it survived; a human reading the census, or a `grep`
/// for `key=value key2=`, did. Checked on the source because the lines
/// are printed only at exit on one backend each.
#[test]
fn no_gc_literal_carries_a_collapsed_line_continuation() {
    let src = concat!(include_str!("../vm_heap/mod.rs"), "\n", include_str!("../vm_heap/compact_copy_plan.rs"), "\n", include_str!("../vm_heap/as_collector.rs"), "\n", include_str!("../vm_heap/soft_ref_policy_free_mb.rs"), "\n", include_str!("../vm_heap/render_tlab_guard_line.rs"));
    let mut bad: Vec<String> = Vec::new();
    for (i, _) in src.match_indices("\"[GC]") {
        let rest = &src[i + 1..];
        let Some(end) = rest.find('"') else { continue };
        for line in rest[..end].lines() {
            let body = line.trim();
            let (mut run, mut widest) = (0usize, 0usize);
            for ch in body.chars() {
                if ch == ' ' {
                    run += 1;
                    widest = widest.max(run);
                } else {
                    run = 0;
                }
            }
            if widest >= 8 {
                bad.push(body.chars().take(72).collect());
            }
        }
    }
    assert!(
        bad.is_empty(),
        "`[GC]` literal(s) with a mid-line run of spaces -- a line \
         continuation lost its trailing backslash: {bad:?}"
    );
}

/// Lines of `src` on which an ordinary (non-raw) string literal carries a
/// run of 12+ spaces between two non-blank characters — the shape a `\`
/// line continuation leaves when its backslash is lost (the newline goes,
/// the next line's indentation stays IN the text). Comments, char
/// literals and raw strings are skipped; a run right after a `\n` / `\t`
/// escape is an intended line break's indentation and is not counted.
fn collapsed_literal_lines(src: &str) -> Vec<usize> {
    literal_space_run_lines(src, false)
}

/// Lines of `src` on which an ordinary string literal carries a `\n`
/// escape followed by a run of 12+ spaces and then text — a `\` line
/// continuation TYPED AS `\n`, so the message prints a line break and the
/// next source line's indentation (gc-common w6-f; the `zgc.rs`
/// remembered-set error and the G1 `CLAMPED a holder's element walk`
/// warning both had it). [`collapsed_literal_lines`] deliberately skips
/// this shape, because a short run after `\n` is an intended break's
/// indentation; 12+ spaces inside GC messages never was.
fn typed_newline_continuation_lines(src: &str) -> Vec<usize> {
    literal_space_run_lines(src, true)
}

/// The shared lexer of the two scanners above: `after_newline_escape`
/// selects which runs count (only those right after `\n`, or only those
/// that are NOT after a `\n` / `\t` escape).
fn literal_space_run_lines(src: &str, after_newline_escape: bool) -> Vec<usize> {
    const MIN_RUN: usize = 12;
    let b = src.as_bytes();
    let n = b.len();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    let blank = |c: u8| c == b' ' || c == b'\n' || c == b'\t';
    let mut out: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 1usize;
            i += 2;
            while i < n && depth > 0 {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if c == b'r'
            && (i == 0 || !ident(b[i - 1]) || (b[i - 1] == b'b' && (i < 2 || !ident(b[i - 2]))))
        {
            let mut j = i + 1;
            while j < n && b[j] == b'#' {
                j += 1;
            }
            if j < n && b[j] == b'"' {
                // `"` then as many `#` as opened it.
                let mut close = vec![b'"'];
                close.resize(j - i, b'#');
                let mut k = j + 1;
                while k < n && !b[k..].starts_with(&close) {
                    k += 1;
                }
                i = (k + close.len()).min(n);
                continue;
            }
        }
        if c == b'\'' {
            if b.get(i + 1) == Some(&b'\\') {
                let mut k = i + 3;
                while k < n && b[k] != b'\'' {
                    k += 1;
                }
                i = (k + 1).min(n);
                continue;
            }
            if i + 2 < n && b[i + 1] < 0x80 && b[i + 2] == b'\'' {
                i += 3;
                continue;
            }
            if i + 1 < n && b[i + 1] >= 0xc0 {
                let mut j = i + 2;
                while j < n && (0x80..=0xbf).contains(&b[j]) {
                    j += 1;
                }
                if j > i + 2 && j < n && b[j] == b'\'' && j - i <= 5 {
                    i = j + 1;
                    continue;
                }
            }
            i += 1; // a lifetime
            continue;
        }
        if c == b'"' {
            let start = i + 1;
            let mut j = start;
            while j < n && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let end = j.min(n);
            let mut k = start;
            while k < end {
                if b[k] != b' ' {
                    k += 1;
                    continue;
                }
                let run_start = k;
                while k < end && b[k] == b' ' {
                    k += 1;
                }
                let escaped_break = run_start >= start + 2
                    && (b[run_start - 2..run_start] == *b"\\n"
                        || b[run_start - 2..run_start] == *b"\\t");
                let escaped_newline =
                    run_start >= start + 2 && b[run_start - 2..run_start] == *b"\\n";
                let counted = if after_newline_escape {
                    escaped_newline
                } else {
                    run_start > start && !blank(b[run_start - 1]) && !escaped_break
                };
                if k - run_start >= MIN_RUN && counted && k < end && !blank(b[k]) {
                    let line = 1 + b[..run_start].iter().filter(|&&x| x == b'\n').count();
                    if out.last() != Some(&line) {
                        out.push(line);
                    }
                }
            }
            i = end + 1;
            continue;
        }
        i += 1;
    }
    out.dedup();
    out
}

/// gc-common w4-f — the widened guard of
/// `common-w3f-collapsed-string-continuations-in-gc-messages-FIXED-20260923.md`:
/// every `.rs` file under `gc/src`, every literal (not only `"[GC]` ones).
/// W4-F collapsed ~140 such lines in `zgc.rs`, `g1.rs`, `gen_heap.rs`,
/// `g1_concurrent.rs`, `g1_cards.rs` and `region.rs`; the three files
/// below belonged to another lane that wave and are frozen at their
/// count (a ratchet: lower it when a file is cleaned). Since w5-f it also
/// scans `gc/tests`.
#[test]
fn no_gc_crate_literal_carries_a_collapsed_line_continuation() {
    const BASELINE: &[(&str, usize)] = &[];
    fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read gc/src") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                collect(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect(&root, &mut files);
    assert!(files.len() > 20, "fixture: gc/src must be found ({root:?})");
    let mut bad: Vec<String> = Vec::new();
    for path in files {
        let src = std::fs::read_to_string(&path).expect("read a gc/src file");
        let lines = collapsed_literal_lines(&src);
        let rel = path
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let allowed = BASELINE
            .iter()
            .find(|(f, _)| *f == rel)
            .map_or(0, |(_, n)| *n);
        if lines.len() > allowed {
            bad.push(format!("{rel}: lines {lines:?} (allowed {allowed})"));
        }
        // gc-common w6-f: the `\n`-typed continuation, no allowance.
        let typed = typed_newline_continuation_lines(&src);
        if !typed.is_empty() {
            bad.push(format!(
                "{rel}: `\\n`-typed continuation on lines {typed:?}"
            ));
        }
    }
    // gc-common w5-f: the crate's integration tests too (their assertion
    // messages are what a red CI run prints). Six such lines were collapsed
    // in w5-f; the one allowed site is a deliberately aligned `println!`
    // (`per-worker scanned             = …` lines up under the `=` of the
    // `per-worker bytes (driver first) = …` line above it).
    const TESTS_ALLOWED: &[(&str, usize)] = &[("g1_w6p_mixed_parallel_equivalence.rs", 1)];
    let tests_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut test_files = Vec::new();
    collect(&tests_root, &mut test_files);
    assert!(
        test_files.len() > 20,
        "fixture: gc/tests must be found ({tests_root:?})"
    );
    for path in test_files {
        let src = std::fs::read_to_string(&path).expect("read a gc/tests file");
        let lines = collapsed_literal_lines(&src);
        let rel = path
            .strip_prefix(&tests_root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let allowed = TESTS_ALLOWED
            .iter()
            .find(|(f, _)| *f == rel)
            .map_or(0, |(_, n)| *n);
        if lines.len() > allowed {
            bad.push(format!("tests/{rel}: lines {lines:?} (allowed {allowed})"));
        }
        let typed = typed_newline_continuation_lines(&src);
        if !typed.is_empty() {
            bad.push(format!(
                "tests/{rel}: `\\n`-typed continuation on lines {typed:?}"
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "string literal(s) with a mid-line run of 12+ spaces -- a `\\` line \
         continuation lost its backslash (end the line with `\\` or use one \
         space): {bad:#?}"
    );
}

#[test]
fn collapsed_literal_scanner_sees_only_literals() {
    let q = '"';
    let gap = " ".repeat(14);
    let lost = format!("let a = {q}first{gap}second{q};\n");
    let comment = format!("// first{gap}second\n");
    let escaped = format!("let b = {q}one\\n{gap}two{q};\n");
    let raw = format!("let c = r#{q}first{gap}second{q}#;\n");
    let src = format!("{comment}{escaped}{raw}{lost}");
    assert_eq!(collapsed_literal_lines(&src), vec![4]);
    // The other scanner sees exactly the `\n`-escaped run, and neither a
    // short indentation after `\n` nor a run after `\t`.
    assert_eq!(typed_newline_continuation_lines(&src), vec![2]);
    let short = format!("let d = {q}one\\n    two{q};\nlet e = {q}one\\t{gap}two{q};\n");
    assert!(typed_newline_continuation_lines(&short).is_empty());
}
