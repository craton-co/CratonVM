// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Census of the `VmHeap` dispatcher: which backend arms DELEGATE to the heap
//! they bind, and which answer a CONSTANT.
//!
//! # The defect this measures
//!
//! `gc/src/vm_heap.rs` is ~6k lines of three-armed `match self`, and the
//! recurring defect there is not a wrong arm — it is an arm that answers a
//! constant (`false`, `0`, `None`, `Vec::new()`, `{}`) long after the backend
//! behind it grew the thing the constant denied. The file's own comments record
//! seven instances, all on one backend, all with a half-life of months, and all
//! found by a human reading the file end to end:
//!
//! * `young_spill_pressure` — hardwired `false`; the collector `abort()`ed on a
//!   heap full of garbage.
//! * `note_young_spill_pressure` / `clear_young_spill_pressure` — no-ops under
//!   a `TODO(zgc)` that had already been discharged.
//! * `enable_gc_logging` — printed a claim instead of enabling anything, so
//!   `--verbose:gc -XX:+UseZGC` produced nothing for a whole run.
//! * `conservative_addr_span` — `None` on a premise that was false when it was
//!   written; its own comment calls that "the reason this went unfixed".
//! * `get_array_element_unboxing` — `Stream.mapToLong(..).toArray()` returned
//!   zeros under `-XX:+UseZGC`.
//! * `reclaimed_hole_at` — `None` on G1's reason, which did not apply.
//! * `is_addr_live` — the loose predicate, which was unsound.
//!
//! That is not a review problem, it is a missing instrument. This is the
//! instrument.
//!
//! # What this is NOT, deliberately
//!
//! It does **not** ratchet. There is no checked-in baseline and no assertion
//! on the constant set, because a gate that asserts on day one against a
//! number nobody has read is a chore with a deadline, and this tree already has
//! instances of exactly that. The census is the first end-to-end answer to "how
//! much of this dispatcher is inert, on which backend" — the number has to be
//! *read* before it can be frozen, and freezing it is the next step, not this
//! one.
//!
//! The only assertions here are vacuity floors on the PARSER: a source scanner
//! that silently matches nothing reports a clean census of a file it never
//! read, which is the failure mode that makes a source-scanning gate worse than
//! no gate.
//!
//! # It is a parser, so it is wrong in both directions
//!
//! Known limitations, stated rather than discovered later:
//!
//! * `//` is treated as a comment start even inside a string literal.
//! * Braces are counted without tracking string or char literals, so a lone
//!   `'{'` in a literal would skew one arm's extent.
//! * Block comments (`/* */`) are not stripped.
//! * An arm classified `UNCLEAR` is the parser declining to guess, and is
//!   reported as its own column rather than folded into either answer.
//!
//! Run it with output:
//!
//! ```text
//! cargo test -p cratonvm-gc --test vm_heap_dispatch_census -- --nocapture
//! ```

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The dispatcher's source, embedded at compile time so the census cannot read
/// a stale copy, depend on the working directory, or quietly find nothing.
const SOURCE: &str = include_str!("../src/vm_heap.rs");

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Class {
    /// The arm calls something — a method on the bound heap, a helper, a macro.
    Delegating,
    /// The arm answers a literal, an empty collection, a `Default`, or nothing
    /// at all. This is the column the review is about.
    Constant,
    /// The parser declines to guess.
    Unclear,
}

struct Arm {
    /// `Generational`, `G1`, `Zgc`, `A+B` for an or-pattern, or
    /// `<catch-all ...>` for `_` / a binding.
    backend: String,
    class: Class,
    /// The arm carries a `#[cfg(...)]` — `Zgc` arms all do.
    cfg_gated: bool,
    body: String,
}

struct FnCensus {
    name: String,
    line: usize,
    arms: Vec<Arm>,
}

fn strip_line_comment(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

fn brace_delta(s: &str) -> i32 {
    let mut d = 0i32;
    for c in s.chars() {
        match c {
            '{' => d += 1,
            '}' => d -= 1,
            _ => {}
        }
    }
    d
}

fn fn_name(trimmed: &str) -> Option<String> {
    let rest = trimmed
        .strip_prefix("pub fn ")
        .or_else(|| trimmed.strip_prefix("pub(crate) fn "))
        .or_else(|| trimmed.strip_prefix("pub unsafe fn "))
        .or_else(|| trimmed.strip_prefix("unsafe fn "))
        .or_else(|| trimmed.strip_prefix("fn "))?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Which backend an arm pattern names. An or-pattern yields `A+B`; a `_` or a
/// binding yields a `<catch-all ..>` label, which is itself worth seeing — a
/// catch-all is how two backends get one answer without either being named.
fn backend_of(pat: &str) -> String {
    let p = pat.trim();
    let mut names: Vec<String> = Vec::new();
    let mut rest = p;
    while let Some(i) = rest.find("VmHeap::") {
        let after = &rest[i + "VmHeap::".len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            names.push(name);
        }
        rest = after;
    }
    if names.is_empty() {
        format!("<catch-all {p}>")
    } else {
        names.join("+")
    }
}

/// Is `s` a literal, an empty collection, a `Default`, or a constant path —
/// i.e. an answer that no backend was asked for?
fn is_literalish(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return true;
    }
    const BARE: &[&str] = &[
        "()",
        "{}",
        "false",
        "true",
        "None",
        "Vec::new()",
        "vec![]",
        "Default::default()",
        "String::new()",
        "Self::default()",
        "HashMap::new()",
        "BTreeMap::new()",
        "HashSet::new()",
    ];
    if BARE.contains(&s) {
        return true;
    }
    // A plain numeric literal, with or without a type suffix (`0`, `0u64`,
    // `-1`, `0.0`).
    let first = s.chars().next().unwrap_or(' ');
    if (first.is_ascii_digit() || first == '-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return true;
    }
    // `Some(<literal>)` / `Ok(<literal>)`.
    for wrapper in ["Some(", "Ok(", "Err("] {
        if let Some(inner) = s.strip_prefix(wrapper).and_then(|r| r.strip_suffix(')')) {
            return is_literalish(inner);
        }
    }
    // A tuple of literals: `(0, 0)`.
    if s.starts_with('(') && s.ends_with(')') {
        let inner = &s[1..s.len() - 1];
        if !inner.is_empty() && inner.split(',').all(|p| is_literalish(p.trim())) {
            return true;
        }
    }
    // A constant PATH with no call and no field access: `usize::MAX`,
    // `ObjectRef::NULL`. A bare lowercase identifier is excluded on purpose —
    // that is a binding being passed through, not a constant.
    if !s.contains('(') && !s.contains('.') && s.contains("::") {
        return s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':');
    }
    false
}

fn classify(body: &str) -> Class {
    let mut b = body.trim();
    b = b.trim_end_matches(',').trim();
    if b.starts_with('{') && b.ends_with('}') && b.len() >= 2 {
        b = b[1..b.len() - 1].trim();
        b = b.trim_end_matches(';').trim();
    }
    if b.is_empty() || is_literalish(b) {
        return Class::Constant;
    }
    // A call, a macro, or a `?` is work handed to somebody. That "somebody" is
    // almost always the bound heap; when it is a free function the arm is still
    // not answering out of thin air, which is what this census is looking for.
    if b.contains('(') || b.contains('!') || b.contains('?') || b.contains('.') {
        return Class::Delegating;
    }
    Class::Unclear
}

/// Parse the arms of the `match self {` opening at `lines[match_line]`.
/// Returns the arms and the index of the line that closed the block.
fn parse_arms(lines: &[&str], match_line: usize) -> (Vec<Arm>, usize) {
    let mut arms: Vec<Arm> = Vec::new();
    let mut depth = 0i32;
    let mut pat: Option<String> = None;
    let mut pending_pat = String::new();
    let mut body = String::new();
    let mut cfg_gated = false;
    let mut pending_cfg = false;
    let mut j = match_line + 1;

    while j < lines.len() {
        let t = strip_line_comment(lines[j]).trim();
        if t.is_empty() {
            j += 1;
            continue;
        }

        if pat.is_none() {
            if pending_pat.is_empty() && t.starts_with('}') {
                return (arms, j);
            }
            if t.starts_with('#') {
                if t.contains("cfg(") {
                    pending_cfg = true;
                }
                j += 1;
                continue;
            }
            match t.find("=>") {
                Some(k) => {
                    let mut p = std::mem::take(&mut pending_pat);
                    p.push_str(&t[..k]);
                    pat = Some(p.trim().to_string());
                    cfg_gated = pending_cfg;
                    pending_cfg = false;
                    body = t[k + 2..].trim().to_string();
                    depth = brace_delta(&body);
                }
                None => {
                    // A pattern spanning more than one line.
                    pending_pat.push_str(t);
                    pending_pat.push(' ');
                    j += 1;
                    continue;
                }
            }
        } else {
            body.push(' ');
            body.push_str(t);
            depth += brace_delta(t);
        }

        // An arm is finished when its braces balance AND its text ends the way
        // an arm ends. Without the second half, `=> h` followed by a chained
        // `.foo(),` on the next line would be read as an arm answering `h`.
        let ends_cleanly = {
            let bt = body.trim_end();
            bt.ends_with(',') || bt.ends_with('}')
        };
        if depth < 0 {
            // The arm ran into the match's own closing brace: a last arm with
            // no trailing comma. Trim it back off and stop here.
            let trimmed = body.trim_end().trim_end_matches('}').trim().to_string();
            let p = pat.take().unwrap_or_default();
            arms.push(Arm {
                backend: backend_of(&p),
                class: classify(&trimmed),
                cfg_gated,
                body: trimmed,
            });
            return (arms, j);
        }
        if depth == 0 && ends_cleanly {
            let p = pat.take().unwrap_or_default();
            arms.push(Arm {
                backend: backend_of(&p),
                class: classify(&body),
                cfg_gated,
                body: body.trim().to_string(),
            });
            body.clear();
        }
        j += 1;
    }
    (arms, lines.len())
}

/// Every `match self` over `VmHeap`, plus a count of the `dispatch!` bodies —
/// which are uniformly delegating on all three arms and so are the part of the
/// surface this census has nothing to say about.
fn census(src: &str) -> (Vec<FnCensus>, usize) {
    let lines: Vec<&str> = src.lines().collect();
    let mut out: Vec<FnCensus> = Vec::new();
    let mut dispatch_bodies = 0usize;
    let mut current_fn = String::from("<top level>");
    let mut current_fn_line = 0usize;
    let mut i = 0usize;

    while i < lines.len() {
        let t = strip_line_comment(lines[i]).trim();
        if let Some(name) = fn_name(t) {
            current_fn = name;
            current_fn_line = i + 1;
        }
        if t.contains("dispatch!(") {
            dispatch_bodies += 1;
        }
        if t.ends_with("match self {") {
            let (arms, end) = parse_arms(&lines, i);
            // Only the dispatcher's own matches. A `match self` over some other
            // enum defined in this file names no `VmHeap::` variant and is not
            // what this census is about.
            if arms.iter().any(|a| !a.backend.starts_with("<catch-all")) {
                out.push(FnCensus {
                    name: current_fn.clone(),
                    line: current_fn_line,
                    arms,
                });
            }
            i = end;
        }
        i += 1;
    }
    (out, dispatch_bodies)
}

#[test]
fn vm_heap_dispatch_census() {
    let (functions, dispatch_bodies) = census(SOURCE);

    let mut report = String::new();
    let _ = writeln!(report, "\n=== VmHeap dispatcher census ===");
    let _ = writeln!(
        report,
        "source: gc/src/vm_heap.rs ({} lines)",
        SOURCE.lines().count()
    );

    // Per-backend tallies, and the reviewable list: every CONSTANT arm.
    let mut tally: BTreeMap<String, [usize; 3]> = BTreeMap::new();
    let mut constant_arms: Vec<(String, usize, String, String)> = Vec::new();
    let mut unclear_arms: Vec<(String, usize, String, String)> = Vec::new();
    let mut total_arms = 0usize;

    for f in &functions {
        for a in &f.arms {
            total_arms += 1;
            let slot = tally.entry(a.backend.clone()).or_insert([0; 3]);
            match a.class {
                Class::Delegating => slot[0] += 1,
                Class::Constant => slot[1] += 1,
                Class::Unclear => slot[2] += 1,
            }
            let excerpt: String = a.body.chars().take(72).collect();
            // A `#[cfg]`-gated arm is worth flagging: it is the shape every ZGC
            // arm has, and a cfg'd-out arm answers nothing at all in a build
            // without the feature.
            let backend = if a.cfg_gated {
                format!("{} [cfg]", a.backend)
            } else {
                a.backend.clone()
            };
            if a.class == Class::Constant {
                constant_arms.push((f.name.clone(), f.line, backend, excerpt));
            } else if a.class == Class::Unclear {
                unclear_arms.push((f.name.clone(), f.line, backend, excerpt));
            }
        }
    }

    let _ = writeln!(
        report,
        "\n{} functions with a `match self`, {} arms, plus {} `dispatch!` \
         bodies (uniformly delegating, not classified here).",
        functions.len(),
        total_arms,
        dispatch_bodies
    );

    let _ = writeln!(report, "\n-- arms by backend --");
    let _ = writeln!(
        report,
        "{:<28} {:>10} {:>9} {:>8}",
        "backend", "DELEGATING", "CONSTANT", "UNCLEAR"
    );
    for (backend, counts) in &tally {
        let _ = writeln!(
            report,
            "{:<28} {:>10} {:>9} {:>8}",
            backend, counts[0], counts[1], counts[2]
        );
    }

    let _ = writeln!(
        report,
        "\n-- CONSTANT arms ({}) — the column this census exists for --",
        constant_arms.len()
    );
    for (f, line, backend, excerpt) in &constant_arms {
        let _ = writeln!(
            report,
            "  {f} (vm_heap.rs:{line})  {backend}  =>  {excerpt}"
        );
    }

    let _ = writeln!(
        report,
        "\n-- UNCLEAR arms ({}) — the parser declining to guess --",
        unclear_arms.len()
    );
    for (f, line, backend, excerpt) in &unclear_arms {
        let _ = writeln!(
            report,
            "  {f} (vm_heap.rs:{line})  {backend}  =>  {excerpt}"
        );
    }

    let _ = writeln!(
        report,
        "\nNo baseline, no ratchet: read the CONSTANT column, then freeze it.\n"
    );
    println!("{report}");

    // ---- vacuity floors on the PARSER, not a ratchet on the tree ----------
    //
    // These numbers are floors an empty or broken scan cannot clear, set far
    // below the real census so they can never become a chore with a deadline.
    // A source-scanning gate that matches nothing prints a clean report of a
    // file it never read; that is the one failure this test does assert on.
    assert!(
        functions.len() >= 10,
        "the census found only {} `match self` functions in a 6k-line \
         dispatcher — the PARSER is broken, not the file clean",
        functions.len()
    );
    assert!(
        total_arms >= 30,
        "the census found only {total_arms} arms — the parser is broken"
    );
    let delegating: usize = tally.values().map(|c| c[0]).sum();
    let constant: usize = tally.values().map(|c| c[1]).sum();
    assert!(
        delegating > 0 && constant > 0,
        "the classifier put every arm in one column ({delegating} delegating, \
         {constant} constant), so it is not classifying"
    );
}
