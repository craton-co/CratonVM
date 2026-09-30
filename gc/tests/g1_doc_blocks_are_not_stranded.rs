//! The lint that
//! `docs/internal/fixed-bugs/g1-doc-blocks-stranded-on-the-wrong-function-FIXED-20260921.md`
//! asked for, and the ratchet that retired it.
//!
//! # The defect this ratchets
//!
//! `gc/src/g1.rs` is 34k lines and its functions have been moved a great deal.
//! When a function moves, its `///` block does not go with it: it stays where
//! it was and glues itself to whatever item now follows. Rust accepts this
//! silently — several `///` runs in a row, with or without blank lines between
//! them, are ONE doc comment on the next item — so the result is one item
//! carrying several unrelated contracts and several items carrying none.
//!
//! That is not a cosmetic problem here. Every one of those blocks is a
//! correctness contract (`eager_reclaim_humongous_locked`'s four gates against
//! a use-after-free, `verify_no_dangling_into_cset`'s UAF precondition,
//! `retire_forwards`' ordering obligation), and a contract rendered on the
//! wrong item invites the reader to believe a guarantee the item in front of
//! them does not make. The 2026-09-21 sweep that retired that page found
//! FIFTEEN of them outside `mod tests` and three more inside it; four of the
//! eighteen also carried a stale claim, which is why a move is never a pure
//! cut-and-paste.
//!
//! # Why a text scan and not a clippy lint
//!
//! `clippy::doc_markdown` does not see this and no rustc lint does either: the
//! construct is well-formed Rust. The page's own "what would close it" names a
//! custom check, and a structural property closes by becoming a ratchet rather
//! than by being re-argued.
//!
//! # The two shapes, and why the second one exists at all
//!
//! 1. **Run-splice.** `/// …sentence.` immediately followed by `/// Capital…`
//!    with no blank `///` between them — a new summary sentence starting
//!    exactly at a line boundary inside what looks like one paragraph. Ordinary
//!    prose in this file wraps, so a continuation line starts mid-sentence;
//!    this shape is the seam where two doc comments were concatenated. Scored,
//!    because the seam alone has ~144 instances of which almost all are
//!    ordinary paragraph breaks: a splice is reported only when the text AFTER
//!    it names the item it is attached to and the text BEFORE it does not.
//! 2. **Blank-line splice.** A `///` run, a genuinely blank line, then another
//!    `///` run, then the item. Both runs still attach to that item. This shape
//!    needs no scoring — a doc comment interrupted by a blank line is always
//!    two doc comments that were never meant to meet.

/// How many name-stems of `name` occur in `text`.
///
/// Stems are the words of the identifier, truncated to five characters so
/// `collect` matches "collection" and `finalizer` matches "finalizers". Words
/// of three characters or fewer and English function words are dropped: they
/// match everything and so discriminate nothing.
///
/// `snake_case` AND `camelCase` are split, and everything is compared in lower
/// case, because the items this file has to score are not all `fn`s: the
/// stranded blocks the sweep found sat on `G1_YOUNG_PARALLEL` and on
/// `EvacShard` as readily as on `phase4_regions_to_walk`, and a matcher that
/// only understood lower-`snake_case` scored both of those at zero against
/// their own documentation.
fn stem_coverage(text: &str, name: &str) -> f64 {
    const STOP: &[&str] = &[
        "with", "from", "into", "this", "that", "self", "and", "for", "the",
    ];
    // `EvacShard` -> `Evac_Shard`, leaving `G1_YOUNG_PARALLEL` alone (a
    // lower-to-upper transition is what marks a camel hump; an all-caps run
    // has none).
    let mut split = String::with_capacity(name.len() + 8);
    for (i, c) in name.char_indices() {
        if i > 0 && c.is_ascii_uppercase() {
            let prev = name[..i].chars().next_back().unwrap_or('_');
            if prev.is_ascii_lowercase() || prev.is_ascii_digit() {
                split.push('_');
            }
        }
        split.push(c.to_ascii_lowercase());
    }
    let stems: Vec<&str> = split
        .split('_')
        .filter(|w| w.len() > 2 && !STOP.contains(w))
        .map(|w| if w.len() > 5 { &w[..5] } else { w })
        .collect();
    if stems.is_empty() {
        return 0.0;
    }
    let lower = text.to_ascii_lowercase();
    let hit = stems.iter().filter(|s| lower.contains(*s)).count();
    hit as f64 / stems.len() as f64
}

/// The name of the item a doc block starting at `i` is attached to, skipping
/// the rest of its own doc lines, its attributes and ordinary `//` comments.
///
/// Returns `None` for an item this check has no name for (a struct field, a
/// closing brace, an `impl` header) — those are not scored.
fn item_name_after(lines: &[&str], i: usize) -> Option<String> {
    for line in lines.iter().skip(i) {
        let t = line.trim_start();
        if t.starts_with("///") || t.starts_with("#[") || t.is_empty() || t.starts_with("//") {
            continue;
        }
        // `pub(crate) unsafe fn foo(`, `static FOO:`, `struct Foo {`, …
        let mut words = t
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|w| !w.is_empty());
        let mut prev_was_keyword = false;
        for w in words.by_ref() {
            if prev_was_keyword {
                return Some(w.to_string());
            }
            match w {
                "fn" | "struct" | "enum" | "trait" | "type" | "static" | "const" => {
                    prev_was_keyword = true;
                }
                "pub" | "crate" | "in" | "unsafe" | "async" | "extern" | "mut" | "default" => {}
                _ => return None,
            }
        }
        return None;
    }
    None
}

/// Does `line` open a new sentence — a capital letter, a backtick, or a
/// markdown link — rather than continue a wrapped one?
fn opens_a_sentence(body: &str) -> bool {
    let s = body.trim_start();
    let Some(c) = s.chars().next() else {
        return false;
    };
    if "*-|#>".contains(c) {
        // A list item, a table row or a heading: structure inside one doc,
        // never the seam between two.
        return false;
    }
    c.is_ascii_uppercase() || c == '`' || c == '['
}

/// `Some(body)` when `line` is a `///` doc line, with the marker stripped.
fn doc_body(line: &str) -> Option<&str> {
    let t = line.trim_start();
    t.strip_prefix("///").map(|b| b.trim_start_matches(' '))
}

/// Index of the first line of the `///` run that `i` belongs to.
fn run_start(lines: &[&str], i: usize) -> usize {
    let mut k = i;
    while k > 0 && doc_body(lines[k - 1]).is_some() {
        k -= 1;
    }
    k
}

/// Seams that are NOT stranded blocks, each verified by reading.
///
/// Keyed by the item the splice sits above and the first words after the seam,
/// so an entry cannot go on silently covering a DIFFERENT splice that later
/// appears above the same item. Keep this list short: every entry is a place
/// the heuristic is admitted to be wrong, and the fix for a real finding is to
/// move the block, never to add a line here.
const ACCEPTED: &[(&str, &str)] = &[
    // `region_type`'s own doc, section "Why `Acquire` …": the sentence after
    // the seam continues the publication argument about `region_type` itself.
    (
        "region_type",
        "`SharedEvac::tlab_alloc` claims a Free region",
    ),
    // `conservative_addr_span`'s own doc: a "see also" line after its summary.
    (
        "conservative_addr_span",
        "See [`crate::gen_heap::GenerationalHeap::conservative_addr_span`]",
    ),
    // `uncommit_to_bytes`' own doc: the return-value line after its summary.
    ("uncommit_to_bytes", "Returns the bytes released."),
    // `mod tests`. This test's own doc: the paragraph after the seam is the
    // "this is a live defect" elaboration of the summary above it, not a
    // second doc — it names `empty_jit_publication` because that is the state
    // the test constructs.
    (
        "a_live_compiled_frame_with_an_empty_publication_still_evacuates_by_default",
        "`empty_jit_publication()` is true",
    ),
];

fn is_accepted(item: &str, after: &str) -> bool {
    ACCEPTED
        .iter()
        .any(|(i, a)| *i == item && after.starts_with(a))
}

/// `gc/src/g1.rs`.
///
/// The WHOLE file, `mod tests` included. The page that asked for this lint
/// hand-verified only the non-test half, but the defect does not respect that
/// boundary — the 2026-09-21 sweep found three more inside `mod tests`. Two
/// came from one pile-up: three doc blocks stacked on
/// `a_span_referenced_from_a_scanned_young_object_is_marked_not_walked`, which
/// left `a_young_pause_reclaims_a_humongous_span_nothing_references` and
/// `a_humongous_span_held_by_an_untouched_old_object_survives_a_narrow_pause`
/// with nothing at all; the third had
/// `the_per_object_screen_skips_the_clean_objects_and_finds_the_dirty_one`'s
/// doc sitting on the JIT-pinned test that follows it. A test's doc says which
/// case it pins, and a reader deciding whether a case is covered reads it the
/// same way they read a contract.
fn g1_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("g1.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Shape 1 — a doc comment spliced onto another one at a line boundary.
///
/// A seam is reported only when the text after it names the item and the text
/// before it does not, which is precisely "the block in front describes
/// something else". Every one of the fifteen instances the 2026-09-21 sweep
/// repaired scored this way; the ~144 ordinary paragraph breaks in the same
/// file do not, because their opening text is about the same item as the rest
/// of the block.
#[test]
fn no_doc_block_is_spliced_onto_the_item_below_it() {
    let text = g1_source();
    let lines: Vec<&str> = text.lines().collect();
    let mut findings: Vec<String> = Vec::new();

    for i in 1..lines.len() {
        let (Some(here), Some(prev)) = (doc_body(lines[i]), doc_body(lines[i - 1])) else {
            continue;
        };
        if here.is_empty() || !prev.trim_end().ends_with('.') {
            continue;
        }
        if prev.trim_start().starts_with(['*', '-', '|', '>']) || !opens_a_sentence(here) {
            continue;
        }
        let Some(item) = item_name_after(&lines, i) else {
            continue;
        };
        if is_accepted(&item, here) {
            continue;
        }
        let start = run_start(&lines, i);
        let head_before: String = (start..i)
            .take(3)
            .filter_map(|k| doc_body(lines[k]))
            .collect::<Vec<_>>()
            .join(" ");
        let head_after: String = (i..(i + 3).min(lines.len()))
            .filter_map(|k| doc_body(lines[k]))
            .collect::<Vec<_>>()
            .join(" ");
        let before = stem_coverage(&head_before, &item);
        let after = stem_coverage(&head_after, &item);
        if after > before && after >= 0.5 {
            findings.push(format!(
                "  g1.rs:{} — the doc block starting at line {} is attached to `{}` but its \
                 opening text is about something else:\n      block opens: {}\n      seam at {}: {}",
                i + 1,
                start + 1,
                item,
                head_before.chars().take(96).collect::<String>(),
                i + 1,
                here.chars().take(96).collect::<String>(),
            ));
        }
    }

    assert!(
        findings.is_empty(),
        "{} doc block(s) in gc/src/g1.rs look stranded on the item that happens to \
         follow them.\n\n{}\n\nMove each block onto the item it describes. If a finding is \
         genuinely one doc comment (the seam is an ordinary paragraph break whose first \
         sentence happens to name the item), add it to `ACCEPTED` in this file WITH the \
         reading that established it.",
        findings.len(),
        findings.join("\n"),
    );
}

/// Shape 2 — two `///` runs separated by a blank line before one item.
///
/// No scoring: both runs attach to the item, so a blank line inside what rustc
/// will render as a single doc comment means two doc comments met by accident.
/// `locate_in_object_grid`'s doc was stranded on `grid_closes_on_cursor` in
/// exactly this shape, and the run-splice check above cannot see it.
#[test]
fn no_two_doc_runs_are_separated_by_a_blank_line() {
    let text = g1_source();
    let lines: Vec<&str> = text.lines().collect();
    let mut findings: Vec<String> = Vec::new();

    for i in 1..lines.len().saturating_sub(1) {
        if !lines[i].trim().is_empty() {
            continue;
        }
        let (Some(above), Some(below)) = (doc_body(lines[i - 1]), doc_body(lines[i + 1])) else {
            continue;
        };
        findings.push(format!(
            "  g1.rs:{} — a blank line splits one doc comment in two; both halves still \
             attach to the item below.\n      above: {}\n      below: {}",
            i + 1,
            above.chars().take(96).collect::<String>(),
            below.chars().take(96).collect::<String>(),
        ));
    }

    assert!(
        findings.is_empty(),
        "{} blank-line-split doc comment(s) in gc/src/g1.rs.\n\n{}\n\nThe upper half almost \
         certainly belongs to a function that moved away. Move it onto that function; if the \
         two halves really are one doc, delete the blank line (use `///` to space paragraphs).",
        findings.len(),
        findings.join("\n"),
    );
}

/// The items the page named as carrying no doc of their own, and the ones the
/// 2026-09-21 sweep found beside them, now each carry one.
///
/// A named list rather than "every item is documented": most of the ~100
/// undocumented fns in this file are trait-impl members whose contract lives on
/// the trait, or two-line accessors. These eighteen are the ones whose
/// contracts were found sitting on a different item, and the property worth
/// freezing is that they did not go back — which the two structural checks
/// above cannot say on their own, because a block deleted outright leaves no
/// seam for them to find.
#[test]
fn the_repaired_functions_still_carry_their_own_doc() {
    let text = g1_source();
    let lines: Vec<&str> = text.lines().collect();

    // (item signature as it appears in the source, a phrase from its doc)
    const REPAIRED: &[(&str, &str)] = &[
        (
            "fn parallel_evac_enabled() -> bool {",
            "CRATONVM_G1_PARALLEL_EVAC",
        ),
        ("fn verify_sweep_pauses() -> usize {", "SWEEP PERIOD"),
        (
            "pub static EVAC_REF_REJECTED: AtomicUsize",
            "did not look like live object headers",
        ),
        (
            "struct EvacShard {",
            "One evacuation worker's private results",
        ),
        ("fn evacuate_object(", "Evacuate a single object"),
        ("fn retire_forwards(", "clear the forwarding tag"),
        ("fn locate_in_object_grid(", "OWN object grid"),
        (
            "fn report_pending_corrupt_holders(",
            "PENDING_CORRUPT_HOLDERS",
        ),
        (
            "fn update_references_in_regions(",
            "rewrite interior references",
        ),
        ("fn verify_no_dangling_into_cset(", "dangling-pointer"),
        ("fn push_gray_or_mark(", "Re-gray the relocated copy"),
        ("fn card_scan_snapshot(", "four card-screen counters"),
        (
            "fn jit_pinned_region_set(",
            "does NOT mean this returns empty",
        ),
        ("pub fn is_object_address(", "Conservative validity check"),
        ("fn gap_filler_len(", "GAP sentinel probe"),
        // …and the three inside `mod tests`.
        (
            "fn a_young_pause_reclaims_a_humongous_span_nothing_references() {",
            "The point of the feature",
        ),
        (
            "fn a_humongous_span_held_by_an_untouched_old_object_survives_a_narrow_pause() {",
            "Ten-findings item 1",
        ),
        (
            "fn the_per_object_screen_skips_the_clean_objects_and_finds_the_dirty_one() {",
            "The per-object screen, and the correctness half",
        ),
    ];

    for (sig, phrase) in REPAIRED {
        let at = lines
            .iter()
            .position(|l| l.contains(sig))
            .unwrap_or_else(|| panic!("`{sig}` is gone from g1.rs; update this list"));
        let start = {
            let mut k = at;
            while k > 0 {
                let t = lines[k - 1].trim_start();
                if t.starts_with("///") || t.starts_with("#[") {
                    k -= 1;
                } else {
                    break;
                }
            }
            k
        };
        let doc: String = (start..at)
            .filter_map(|k| doc_body(lines[k]))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !doc.trim().is_empty(),
            "`{sig}` has no doc comment of its own. Its contract was stranded on another \
             item once already; see docs/internal/fixed-bugs/\
             g1-doc-blocks-stranded-on-the-wrong-function-FIXED-20260921.md",
        );
        assert!(
            doc.contains(phrase),
            "`{sig}`'s doc no longer contains {phrase:?}, so it is probably not the block \
             that was moved onto it. Check that the contract did not drift back onto a \
             neighbouring item.",
        );
    }
}
