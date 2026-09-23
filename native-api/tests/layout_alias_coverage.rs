// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! COVERAGE GATE for the layout-alias census — the detector must keep seeing
//! **every** native object allocation, not just the fabrication funnel's.
//!
//! # The failure this exists to prevent
//!
//! Until 2026-08-12 the detector (`report_layout_alias`) sat on exactly one
//! caller: `try_alloc_concurrent_synthetic` in `native-builtins`. That funnel
//! has ~1,800 call sites, which made it *look* like the allocation. It is not.
//! `native-builtins`, `native-io` and `native-collections` also hold direct
//! `NativeContext::alloc_object` / `try_alloc_object_gc_safe` call sites — ~200
//! of them in production code — and none of them reached the detector. So
//! `CRATONVM_DBG_LAYOUT_ALIAS=1` printed a census that was silently partial, and
//! the lanes downstream read it as coverage.
//!
//! It was not a theoretical partiality. `native-io/src/async_socket.rs`
//! allocates `java/nio/channels/AsynchronousSocketChannel` four slots wide
//! against a class declaring one, and `native-io` is the live owner of that
//! class: **the widest live over-allocation in the workspace was invisible to
//! the instrument built to find over-allocations.** See
//! W7-59-layout-detector-coverage.md and W7-49-slot-index-recensus.md.
//!
//! An instrument that reports clean because it cannot see is worse than no
//! instrument, because a clean report is consumed as evidence. This file makes
//! the next partial migration fail loudly instead of quietly.
//!
//! # The argument this gate mechanises
//!
//! A native crate can create a Java object with a **caller-chosen slot count**
//! only through the `NativeContext` trait — the native crates cannot reach the
//! heap themselves ([`no_native_crate_touches_the_heap_directly`]). Exactly two
//! trait methods take a caller-supplied `num_fields`
//! ([`the_layout_asserting_allocation_surface_is_exactly_two_methods`]), and one
//! of them has no override anywhere, so it terminates in the other
//! ([`try_alloc_object_gc_safe_has_no_override_that_could_route_around_alloc_object`]).
//! That single remaining method's VM implementation carries the observation
//! ([`the_base_allocator_observes`]). Therefore every layout-asserting native
//! allocation in the workspace is observed.
//!
//! Each link is a separate test so a break names which link broke.
//!
//! # How each one fails
//!
//! * Add a third `num_fields`-taking allocation method to `NativeContext` →
//!   [`the_layout_asserting_allocation_surface_is_exactly_two_methods`] fails,
//!   naming it. That is the exact shape of the hole this lane closed: a second
//!   allocation door that the detector does not sit on.
//! * Override `try_alloc_object_gc_safe` in the VM (the trait's own doc comment
//!   invites it) → the override would allocate without passing `alloc_object`,
//!   so the observation is bypassed;
//!   [`try_alloc_object_gc_safe_has_no_override_that_could_route_around_alloc_object`]
//!   fails and tells the author to carry the observation into it.
//! * Delete, rename or refactor away the `layout_alias::observe` call in
//!   `NativeContextImpl::alloc_object` → [`the_base_allocator_observes`] fails.
//!   This is the one that catches a well-meaning hot-path cleanup.
//! * Give a native crate direct heap access →
//!   [`no_native_crate_touches_the_heap_directly`] fails, naming the file.
//! * Re-inline a second copy of the counting/flag/dedup machinery beside the
//!   shared one → [`there_is_exactly_one_detector`] fails. Two detectors that
//!   can disagree is the disease, not the cure.
//!
//! # The second argument: routing is not the same as SEEING
//!
//! Links 1–5 prove that every layout-asserting allocation reaches the census.
//! They say nothing about whether the census has anything to say when it gets
//! there, and W7-73-short-object-blind-spot.md found that for the species it is
//! named after it did not. `classify` returned `None` for `declared == 0`, which
//! is the ONE case in which the base allocator's
//! `slots = num_fields.max(real_fields)` clamp leaves the request alone — hence
//! the one case in which the allocated object can be narrower than the class it
//! is handed out as. Every `under` row describes an object the clamp already
//! widened; the short objects were all in the silent bucket. And the
//! `ClassId::new(0)` sentinel never even got that far: `alloc_object`
//! substitutes `cratonvm/synthetic/AnonymousObject$N`, which declares exactly
//! `N`, so 30 production sites arrived as `classify(n, n)` and printed nothing
//! in either direction.
//!
//! A gate proving routing while the rule at the end of the route is silent is a
//! vacuous green with extra steps. Links 6–10 gate the RULE:
//!
//! * Fold `declared == 0` back into `classify`'s `None` guard, or collapse it
//!   into `Over` → [`a_class_declaring_nothing_is_a_reported_direction`] fails.
//!   A live call, not a source scan.
//! * Drop the `Undeclared` arm from `observe`'s `match` →
//!   [`the_undeclared_direction_reaches_the_wire`] fails.
//! * Add a fast path that returns from `alloc_object` above the observation →
//!   [`no_allocation_door_opens_before_the_census`] fails, quoting the line. The
//!   `anon_class_cache` early return did exactly this until 2026-08-12.
//! * Hoist the `ClassId::new(0)` substitution above the sentinel observation →
//!   [`the_unresolved_class_sentinel_is_observed_before_it_is_substituted`]
//!   fails. After the substitution the widths agree by construction.
//! * Re-open-code the classification in the fabrication funnel →
//!   [`the_fabrication_funnel_uses_the_shared_classify`] fails. Its old inline
//!   predicate carried the `real > 0` exclusion this lane removed, so the
//!   funnel had already drifted from the shared rule on the case that matters.
//! * Add another `alloc_object(ClassId::new(0), N)` site →
//!   [`the_unresolved_class_fallback_population_only_shrinks`] fails. The one
//!   ratchet in this file, and see its doc for why this population earns one
//!   when [`census`] does not.
//! * Inline the appended-slot base back into a literal width at
//!   `alloc_mapped_byte_buffer` or `native_fc_open` →
//!   [`the_appended_slot_allocators_do_not_regress_to_a_literal_width`] fails.
//!   Neither the ratchet nor the census can see that one: the site COUNT does
//!   not move, and the width the detector reports is correct for the object it
//!   allocated. What moves is where the private map lands — back inside the real
//!   class's declared fields — and whether the row can be short at all.
//!
//! # What this gate deliberately does NOT assert
//!
//! It does not ratchet the *number* of allocation sites. Sites are added and
//! removed constantly and a count would be re-baselined on sight, which trains
//! people to re-baseline the whole file. It asserts the *shape*: how many doors
//! there are, and that each is watched. The counts are printed instead
//! ([`census`]) so a reader gets them without re-deriving them.
//!
//! It also cannot see `#[cfg(test)]` allocation sites, and does not try: those
//! run against `MockNativeContext`, which has its own `alloc_object` and never
//! reaches the VM. On 2026-08-12 that was 305 of the 513 raw sites — a number
//! worth knowing before quoting the raw grep.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Crates that implement natives. Every one of them allocates Java objects and
/// none of them may do so except through `NativeContext`.
const NATIVE_CRATES: &[&str] = &[
    "native-builtins",
    "native-io",
    "native-collections",
    "native-builtins-crypto",
    "native-builtins-security",
    "native-awt",
];

/// The two spellings a native uses to allocate an object whose slot count it
/// chose itself. This is the population the census must cover.
const LAYOUT_ASSERTING_METHODS: &[&str] = &["alloc_object", "try_alloc_object_gc_safe"];

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
/// still work. A gate that counts a spelling inside a doc comment reports
/// findings that are not there, and a gate with false positives gets muted.
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

/// Byte offset of the `}` closing the `{` at `open`, or `None` if it never
/// closes.
///
/// **String literals are skipped, and that is not fastidiousness.** The first
/// draft of this file matched braces blind, and `graalvm_compat.rs`'s
/// `#[cfg(test)] mod tests` contains a string with an unbalanced `{`: the span
/// never closed, seven test-only allocation sites were silently counted as
/// production, and the census printed a number that was wrong in the direction
/// that flatters the finding. A scanner whose failure mode is "silently
/// misattribute" is the same disease as a detector that reports clean because it
/// cannot see.
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
// Link 1 — how many layout-asserting doors are there?
// ---------------------------------------------------------------------------

/// Every `NativeContext` method that lets the caller choose the slot count is a
/// door the census has to sit behind. There are two, and the census sits behind
/// the one the other funnels into.
///
/// A third one added without instrumenting it is precisely how the hole this
/// lane closed was opened, so it fails here rather than being discovered by a
/// recensus a month later.
#[test]
fn the_layout_asserting_allocation_surface_is_exactly_two_methods() {
    let registry = strip_comments(&read("native-api/src/registry.rs"));
    let mut found: BTreeSet<String> = BTreeSet::new();
    for (idx, _) in registry.match_indices("num_fields: usize") {
        // Walk back to the `fn` that declares it.
        let head = &registry[..idx];
        let Some(fn_at) = head.rfind("fn ") else {
            continue;
        };
        let after = &registry[fn_at + 3..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        // Only allocation doors: the method must hand back an object.
        let sig_end = registry[fn_at..]
            .find('{')
            .unwrap_or(registry.len() - fn_at)
            + fn_at;
        let sig_end = registry[fn_at..sig_end]
            .find(';')
            .map_or(sig_end, |s| fn_at + s);
        let sig = &registry[fn_at..sig_end];
        if sig.contains("ObjectRef") && sig.contains("->") {
            found.insert(name);
        }
    }
    let expected: BTreeSet<String> = LAYOUT_ASSERTING_METHODS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(
        found, expected,
        "the set of NativeContext methods taking a caller-supplied `num_fields` and \
         returning an object changed.\n\
         Every one of them is a door a native uses to impose its own slot map on a \
         real JDK class, which is the species \
         docs/architecture/natives-over-real-jdk-classes.md section 5 calls heap \
         corruption. A new door must either route through `alloc_object` (which \
         carries the layout-alias observation) or call \
         `cratonvm_native_api::layout_alias::observe` itself — and then be added \
         to LAYOUT_ASSERTING_METHODS here, deliberately.\n\
         See W7-59-layout-detector-coverage.md."
    );
}

// ---------------------------------------------------------------------------
// Link 2 — the second door funnels into the first, and must keep doing so
// ---------------------------------------------------------------------------

/// `try_alloc_object_gc_safe` is observed only because its trait default is
/// `Some(self.alloc_object(..))` and **nothing overrides it**. Its own doc
/// comment says "the real VM implementation overrides this with the actual
/// fallible allocator" — which is not true today, and the day it becomes true
/// the override allocates without passing the observation.
///
/// So this is not a stylistic gate. It is the one link in the argument that a
/// reasonable, well-intentioned change breaks silently.
#[test]
fn try_alloc_object_gc_safe_has_no_override_that_could_route_around_alloc_object() {
    let root = workspace_root();
    let mut definitions: Vec<String> = Vec::new();
    for crate_dir in ["vm", "gc", "native-api", "jit", "libcratonvm"]
        .iter()
        .chain(NATIVE_CRATES.iter())
    {
        for file in rust_sources(&root.join(crate_dir).join("src")) {
            let src = strip_comments(&fs::read_to_string(&file).unwrap_or_default());
            if src.contains("fn try_alloc_object_gc_safe") {
                definitions.push(
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
        definitions,
        vec!["native-api/src/registry.rs".to_string()],
        "`try_alloc_object_gc_safe` is now defined somewhere other than its trait \
         default.\n\
         The default forwards to `alloc_object`, which is where the layout-alias \
         census observes. An override does not, so every allocation taking the \
         override becomes invisible to CRATONVM_DBG_LAYOUT_ALIAS — reopening \
         exactly the hole W7-59 closed.\n\
         If the override is wanted, carry the observation into it: gate on \
         `layout_alias::enabled()`, compare the requested count against \
         `class_num_total_fields`, and call `layout_alias::observe`. Then list the \
         new file here."
    );
}

// ---------------------------------------------------------------------------
// Link 3 — the remaining door is watched
// ---------------------------------------------------------------------------

/// The VM's `alloc_object` must still contain the observation.
///
/// This is the link a hot-path cleanup breaks. `alloc_object` is genuinely hot
/// (a large share of an autobox-heavy HashMap probe sits in it), it already
/// carries a thread-local field-count cache and a TLAB fast path, and the
/// observation looks like something that can be lifted out. It cannot: it is the
/// only place in the workspace where a native's requested slot count and the
/// loaded class's real one are both in hand.
#[test]
fn the_base_allocator_observes() {
    let src = read("vm/src/vm/vm_exec.rs");
    // Bound to a `let`, not inlined into the call: `fn_body` hands back a slice
    // of the stripped source, which would be dropped at the end of the
    // statement if the temporary were never named.
    let stripped = strip_comments(&src);
    let body = fn_body(&stripped, "alloc_object")
        .expect("vm_exec.rs no longer defines `fn alloc_object` — the base allocator moved");
    for needle in ["layout_alias::enabled()", "layout_alias::observe("] {
        assert!(
            body.contains(needle),
            "`NativeContextImpl::alloc_object` no longer contains `{needle}`.\n\
             That call is the whole layout-alias census for every direct \
             `alloc_object` / `try_alloc_object_gc_safe` site in the native crates \
             — roughly 200 production sites that reach no other detector. Without \
             it CRATONVM_DBG_LAYOUT_ALIAS reports only the fabrication funnel and \
             reads clean for the rest, which is how the widest live \
             over-allocation in the workspace \
             (`java/nio/channels/AsynchronousSocketChannel`, 4 slots against a \
             class declaring 1) went unseen.\n\
             It is observation-only and costs one OnceLock load when the flag is \
             off; if it is in the way, move it, do not drop it.\n\
             See W7-59-layout-detector-coverage.md."
        );
    }
    // The comparison must use the count the CALLER asked for, not the clamped
    // one. `slots` is `num_fields.max(real_fields)`; observing `slots` would
    // report nothing in the `under` direction and would be a silent
    // half-blinding of exactly the kind this file exists to prevent.
    let observe_at = body.find("layout_alias::observe(").expect("checked above");
    let clamp_at = body
        .find("let slots = num_fields.max(real_fields);")
        .expect(
            "the clamp `let slots = num_fields.max(real_fields);` changed shape; \
             re-check that the census still observes the UNCLAMPED request",
        );
    assert!(
        observe_at < clamp_at,
        "the layout-alias observation now runs AFTER the slot-count clamp.\n\
         The clamp raises an under-request to the real width, so after it the two \
         counts always agree and the `under` direction — the one the detector has \
         reported since it was written — silently reports nothing."
    );
}

// ---------------------------------------------------------------------------
// Link 4 — no native crate has its own heap
// ---------------------------------------------------------------------------

/// The whole argument rests on native crates having no way to make an object
/// except through `NativeContext`. `native-collections` does depend on
/// `cratonvm-gc` (for external-root registration), so the dependency graph alone
/// does not settle it; the spellings do.
#[test]
fn no_native_crate_touches_the_heap_directly() {
    let root = workspace_root();
    // Terminal object allocators inside the VM/GC. A native crate naming any of
    // these is allocating behind the trait, and therefore behind the census.
    const HEAP_DOORS: &[&str] = &[
        "try_alloc_object_full",
        "alloc_object_shared",
        "heap_alloc_object",
        "GenHeap",
        "mem.heap",
    ];
    let mut offenders: Vec<String> = Vec::new();
    for crate_dir in NATIVE_CRATES {
        for file in rust_sources(&root.join(crate_dir).join("src")) {
            let src = strip_comments(&fs::read_to_string(&file).unwrap_or_default());
            for door in HEAP_DOORS {
                if src.contains(door) {
                    offenders.push(format!(
                        "{} names `{door}`",
                        file.strip_prefix(&root)
                            .unwrap_or(&file)
                            .display()
                            .to_string()
                            .replace('\\', "/")
                    ));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a native crate reaches the heap without going through `NativeContext`:\n  {}\n\
         Such an allocation never passes `NativeContextImpl::alloc_object` and is \
         therefore invisible to the layout-alias census, whatever slot map it \
         imposes on a real JDK class.\n\
         See W7-59-layout-detector-coverage.md.",
        offenders.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// Link 5 — one detector, not two
// ---------------------------------------------------------------------------

/// The counting, the flag, the dedup key and the channel live in
/// `native-api/src/layout_alias.rs`, once. `native-builtins`'
/// `report_layout_alias` is a forwarder.
///
/// Two implementations of one primitive that can disagree is a shape this
/// project has paid for before. The funnel's copy was deleted rather than left
/// beside the shared one, and this keeps it deleted.
#[test]
fn there_is_exactly_one_detector() {
    let root = workspace_root();
    let mut owners: Vec<String> = Vec::new();
    for crate_dir in ["vm", "gc", "native-api", "types"]
        .iter()
        .chain(NATIVE_CRATES.iter())
    {
        for file in rust_sources(&root.join(crate_dir).join("src")) {
            let src = strip_comments(&fs::read_to_string(&file).unwrap_or_default());
            // The signature of "this file decides what the census prints":
            // it reads the flag AND emits a direction.
            if src.contains("CRATONVM_DBG_LAYOUT_ALIAS")
                && (src.contains("direction = \"over\"") || src.contains("direction = \"under\""))
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
        vec!["native-api/src/layout_alias.rs".to_string()],
        "the layout-alias census is emitted from more than one place.\n\
         Two detectors on one primitive drift, and then disagree, and then the \
         reader has to work out which one to believe — which is how this campaign \
         spent a lane on `report_layout_alias` only to find it was watching a \
         fifth of the population. Forward to \
         `cratonvm_native_api::layout_alias::observe` instead of re-implementing."
    );
}

// ---------------------------------------------------------------------------
// Links 6–10 — the SHORT-OBJECT half (W7-73-short-object-blind-spot.md)
//
// Links 1–5 prove every layout-asserting allocation REACHES the census. They say
// nothing about whether the census has anything to say when it gets there, and
// for the one species it is named after it did not: `classify` returned `None`
// for `declared == 0`, which is the only case in which
// `slots = num_fields.max(real_fields)` leaves the request alone — hence the
// only case in which the object can be narrower than the class it is handed out
// as. Every `under` row describes an object the clamp already widened.
//
// A gate that proves routing while the rule at the end of the route is silent is
// the shape of a vacuous green, so these five gate the RULE.
// ---------------------------------------------------------------------------

/// `classify(N, 0)` for `N > 0` must report, not swallow.
///
/// A live call into the crate under test, not a source scan: the arm can be
/// deleted, or `declared == 0` folded back into the `None` guard, without any
/// text this file greps for changing. It also asserts the direction is its OWN
/// variant rather than `Over` — collapsing it into `Over` would claim the object
/// has more slots than its class declares fields, which is a statement about a
/// known layout, and here there is no known layout.
///
/// **How it fails.** Restore the old `if requested == 0 || declared == 0 || ...`
/// guard in `native-api/src/layout_alias.rs` and this goes red on the first
/// assertion, naming the widths of the two worst real sites. (Verified red
/// against a mutated copy that restored that guard: `classify(6, 0)` came back
/// `None` and the assertion fired.)
#[test]
fn a_class_declaring_nothing_is_a_reported_direction() {
    use cratonvm_native_api::layout_alias::{classify, Direction};
    assert_eq!(
        classify(6, 0),
        Some(Direction::Undeclared),
        "`classify` went quiet again for declared == 0.\n\
         That is the ONLY case in which `NativeContextImpl::alloc_object`'s \
         `slots = num_fields.max(real_fields)` clamp does nothing, so it is the \
         only case in which the allocated object can be SHORTER than the class it \
         is handed out as. 6-against-14 is `java/util/zip/ZipEntry`'s \
         `ClassId::new(0)` fallback arm; 2-against-20 is `java/util/regex/Pattern`; \
         5-against-19 is the `java/lang/Thread` mirror, on a path that is not even \
         a fallback.\n\
         Reporting `None` here is not neutrality — a suppressed row and an absent \
         defect are the same bytes to every consumer of this census.\n\
         See W7-73-short-object-blind-spot.md."
    );
    assert_ne!(
        classify(6, 0),
        Some(Direction::Over),
        "`Undeclared` was folded into `Over`. An `over` row asserts the object has \
         more slots than its class has FIELDS — a claim about a known layout. \
         `declared == 0` means there is no known layout to make a claim about, and \
         the three shapes behind it (field-less class, unregistered id, fabricated \
         stub over a wider real class) need different fixes."
    );
    // Unchanged, and asserted here so a future widening cannot quietly start
    // reporting allocations that assert no layout at all.
    assert_eq!(classify(0, 0), None);
    assert_eq!(classify(0, 9), None);
}

/// The detector must still EMIT the third direction, not merely compute it.
///
/// [`a_class_declaring_nothing_is_a_reported_direction`] would still pass if
/// `observe`'s `match` lost its `Undeclared` arm and fell through to `Over`'s
/// message, or if the arm logged at a level nothing collects. This pins the
/// wire format the same way [`there_is_exactly_one_detector`] pins the channel.
#[test]
fn the_undeclared_direction_reaches_the_wire() {
    let src = strip_comments(&read("native-api/src/layout_alias.rs"));
    assert!(
        src.contains("direction = \"undeclared\""),
        "`native-api/src/layout_alias.rs` no longer emits `direction = \"undeclared\"`.\n\
         The classification without the row is a rule nobody can read. The field \
         names (`class`, `requested_fields`, `real_fields`, `direction`, `site`) and \
         the channel are deliberately identical to the other two directions so an \
         existing consumer's filter still works and can opt in.\n\
         See W7-73-short-object-blind-spot.md."
    );
    // The two directions that predate this lane, so widening the census can
    // never be paid for by narrowing it.
    for d in ["direction = \"under\"", "direction = \"over\""] {
        assert!(src.contains(d), "the detector stopped emitting `{d}`");
    }
}

/// Nothing may return from `alloc_object` before the census has looked.
///
/// This is the link that would silently reopen the blind spot, and it is not
/// hypothetical: `alloc_object` already grew one early `return` — the lock-free
/// `anon_class_cache` fast path for `ClassId::new(0)` allocations — which
/// bypassed the observation entirely for the busiest untyped-allocation path in
/// the VM. Another hot-path fast path added above the census would do it again,
/// and the population it hides is exactly the population W7-73 measured: 30
/// production sites, 16 of them requesting fewer slots than the class they name
/// really declares.
///
/// The census does not have to be at the very top — the sentinel observation
/// comes first, then the substitution, then the width observation — it has to be
/// above every exit.
///
/// **How it fails.** Add `if some_fast_path { return obj; }` anywhere above the
/// first `layout_alias::enabled()` in `NativeContextImpl::alloc_object` and this
/// prints the offending line. (Verified red against a mutated copy carrying
/// `if num_fields == 0 { return self.heap_alloc_object(class_id, 0); }` as the
/// method's first statement: the assertion fired and quoted that line.)
#[test]
fn no_allocation_door_opens_before_the_census() {
    let src = read("vm/src/vm/vm_exec.rs");
    let stripped = strip_comments(&src);
    let body = fn_body(&stripped, "alloc_object")
        .expect("vm_exec.rs no longer defines `fn alloc_object` — the base allocator moved");
    let census_at = body.find("layout_alias::enabled()").expect(
        "`NativeContextImpl::alloc_object` no longer gates on `layout_alias::enabled()` \
         — see `the_base_allocator_observes`",
    );
    let mut escapes: Vec<String> = Vec::new();
    for (idx, _) in body[..census_at].match_indices("return") {
        // Word boundary both sides: `returned`, `_return` and friends are not
        // control flow, and a gate with false positives gets muted.
        let before = body[..idx].chars().next_back().unwrap_or(' ');
        let after = body[idx + 6..].chars().next().unwrap_or(' ');
        if before.is_alphanumeric() || before == '_' {
            continue;
        }
        if after.is_alphanumeric() || after == '_' {
            continue;
        }
        let line_no = body[..idx].matches('\n').count() + 1;
        let text: String = body[idx..].chars().take(80).collect();
        let text = text.replace('\n', " ");
        escapes.push(format!("  +{line_no} lines into the body: {}", text.trim()));
    }
    assert!(
        escapes.is_empty(),
        "`NativeContextImpl::alloc_object` can return BEFORE the layout-alias \
         census observes:\n{}\n\
         Every allocation taking that exit is invisible to \
         CRATONVM_DBG_LAYOUT_ALIAS in all three directions, whatever slot map it \
         imposes on a real JDK class. The `anon_class_cache` fast path did exactly \
         this until 2026-08-12 and hid the entire short-object population — 30 \
         production `alloc_object(ClassId::new(0), N)` sites, 16 of them narrower \
         than the class they name.\n\
         The observation is one OnceLock load when the flag is off. Move the fast \
         path BELOW it, or carry an observation into the fast path; do not step \
         over it.\n\
         See W7-73-short-object-blind-spot.md.",
        escapes.join("\n")
    );
}

/// The sentinel must be observed while it is still the sentinel.
///
/// `alloc_object` substitutes `cratonvm/synthetic/AnonymousObject$N` for
/// `ClassId::new(0)`, and that stub declares exactly `N`. So after the
/// substitution every one of these allocations classifies as `classify(n, n)` —
/// agreement — and prints nothing. The observation is only meaningful strictly
/// above `ensure_generated_class`.
///
/// This is the same ordering argument as [`the_base_allocator_observes`]'s
/// observe-before-clamp check, one substitution earlier, and it fails the same
/// way: a well-meaning tidy-up that hoists the class fixup to the top of the
/// method leaves a census that still compiles, still runs, and reports clean.
/// (Verified red against a mutated copy with an `ensure_generated_class` call
/// moved above the observation.)
#[test]
fn the_unresolved_class_sentinel_is_observed_before_it_is_substituted() {
    let src = read("vm/src/vm/vm_exec.rs");
    let stripped = strip_comments(&src);
    let body =
        fn_body(&stripped, "alloc_object").expect("vm_exec.rs no longer defines `fn alloc_object`");
    let observe_at = body.find("layout_alias::UNRESOLVED_CLASS").expect(
        "`NativeContextImpl::alloc_object` no longer observes the `ClassId::new(0)` \
         sentinel under `layout_alias::UNRESOLVED_CLASS`.\n\
         Without it the 30 production `alloc_object(ClassId::new(0), N)` sites in the \
         native crates are invisible in BOTH directions at once: `under` cannot fire \
         because the clamp runs first, and this path cannot fire because the \
         AnonymousObject$N substitution makes the two widths agree by construction.\n\
         See W7-73-short-object-blind-spot.md.",
    );
    let substitute_at = body.find("ensure_generated_class(").expect(
        "the `ClassId::new(0)` substitution changed shape; re-check that the census \
         still observes the UNRESOLVED class id rather than its stand-in",
    );
    assert!(
        observe_at < substitute_at,
        "the sentinel observation now runs AFTER `ensure_generated_class` has \
         substituted `cratonvm/synthetic/AnonymousObject$N` for it.\n\
         That stub declares exactly the requested count, so the census sees \
         `classify(n, n)` — agreement — and reports nothing for every one of these \
         allocations. The caller, meanwhile, tried to resolve a real class, FAILED, \
         and handed the object out as an instance of it anyway."
    );
}

/// The fabrication funnel must use the shared rule, not its own copy.
///
/// W7-59-layout-detector-coverage.md moved the counting, the flag, the dedup key
/// and the channel into one module and asserted *"there is one implementation
/// here"*. That was true of the reporting MACHINERY and false of the DECISION:
/// `try_alloc_concurrent_synthetic` kept `num_fields > 0 && real > 0 &&
/// num_fields != real` inline, which is `classify` open-coded — and its
/// `real > 0` term is exactly the `declared == 0` exclusion this lane removed.
/// The two had already drifted before anyone looked.
///
/// [`there_is_exactly_one_detector`] cannot catch this: that file does not read
/// the flag and does not emit a direction, so it is not an owner by that test's
/// definition. It just decides, in secret, what the owner is allowed to hear.
#[test]
fn the_fabrication_funnel_uses_the_shared_classify() {
    let stripped = strip_comments(&read("native-builtins/src/util_concurrent_ext.rs"));
    let body = fn_body(&stripped, "try_alloc_concurrent_synthetic")
        .expect("`try_alloc_concurrent_synthetic` moved — it is the second observation point");
    assert!(
        body.contains("layout_alias::classify("),
        "the fabrication funnel no longer calls `layout_alias::classify` — it is \
         deciding for itself which allocations the census is allowed to see.\n\
         That is not a style point. The predicate it used until 2026-08-12 \
         (`num_fields > 0 && real > 0 && num_fields != real`) excluded `real == 0`, \
         which is the ONLY case the allocator's `max` clamp leaves alone and \
         therefore the only case in which the object is genuinely SHORT. One \
         primitive, one implementation; forward to `classify` and let the shared \
         rule widen for everyone at once.\n\
         See W7-73-short-object-blind-spot.md."
    );
    assert!(
        !body.contains("real > 0"),
        "the fabrication funnel has re-acquired a `real > 0` guard. `real == 0` is \
         the short-object case, not the uninteresting one — see \
         W7-73-short-object-blind-spot.md."
    );
}

/// Link 12 — the two allocators whose width is DERIVED must keep deriving it.
///
/// `native-io/src/lib.rs`'s `alloc_mapped_byte_buffer` and `native_fc_open` are
/// the only two sites in the workspace whose requested slot count comes from
/// `appended_slots::base_for_class` (directly, or through
/// `synthetic_file_channel::alloc_slots`, which is `base_for_class(…) + 2`).
/// That is not a stylistic choice and it is not only about the alias: it is what
/// takes both rows OUT of the short population this file ratchets.
///
/// The argument, which is W7-74-short-object-repairs.md §1.3 generalised from
/// `MappedByteBuffer` to both sites. `base_for_class` resolves the class through
/// the same `ensure_class_initialized` the surrounding `match` scrutinises, and
/// answers 0 when it fails or when the class is a fabricated stub. So the two
/// halves of a "short by N" claim can never both hold in one execution:
///
/// * on an image where the class really declares `base`, the request is
///   `base + private_width` — an `over` row, the appended-slot idiom's
///   signature, and the object carries every declared field;
/// * on an image where the request collapses to `private_width`, the base is 0,
///   which means the class did not resolve — and there is then no declared width
///   for the object to be short against.
///
/// Replace either derivation with the literal it replaced (`12`/`2` for
/// `MappedByteBuffer`, `2` for `FileChannel`) and both properties go at once:
/// the private map lands back inside the real class's declared fields — the
/// `MMAP_REGISTRY` id in `nativeByteOrder` and the writable flag in `fd`
/// (W7-68 §3.1), the fd in `closeLock` and the file position in `closed`, which
/// real `AbstractInterruptibleChannel.isOpen()` reads (W7-72 §2.2) — and the two
/// rows silently re-enter the short column without the ratchet's total moving,
/// because the site count is unchanged. **A literal here is invisible to every
/// other gate in this file.**
///
/// *Red when*: a "simplification" pass inlines the base, or a new `alloc_object`
/// call with a bare numeric width is added to either body. Deliberately scans
/// the whole body rather than one line, because a fixed line band in a
/// source-witness test goes stale within a wave.
#[test]
fn the_appended_slot_allocators_do_not_regress_to_a_literal_width() {
    let stripped = strip_comments(&read("native-io/src/lib.rs"));
    for (func, derivation) in [
        (
            "alloc_mapped_byte_buffer",
            "appended_slots::base_for_class(",
        ),
        ("native_fc_open", "synthetic_file_channel::alloc_slots("),
    ] {
        let body = fn_body(&stripped, func).unwrap_or_else(|| {
            panic!(
                "`{func}` is gone from native-io/src/lib.rs. It is one of the two \
                 appended-slot allocators; if it moved, move this gate with it — \
                 see W7-74-short-object-repairs.md §1.3."
            )
        });
        assert!(
            body.contains(derivation),
            "`{func}` no longer derives its allocation width through \
             `{derivation}`. The derivation is what keeps the private slots ABOVE \
             every field the class declares, and what makes the site's census row \
             structurally unable to be `short`. See W7-72-ssc-socket-and-filechannel.md \
             and W7-68-live-under-allocations.md §3.1."
        );
        let needle = "alloc_object(";
        for (idx, _) in body.match_indices(needle) {
            let prev = body[..idx].chars().next_back().unwrap_or(' ');
            if prev.is_alphanumeric() || prev == '_' {
                continue;
            }
            let open = idx + needle.len() - 1;
            let Some(args) = paren_args(body, open) else {
                continue;
            };
            let parts = split_top_level(args);
            let Some(width) = parts.get(1) else {
                continue;
            };
            let width = width.trim();
            assert!(
                width.is_empty() || !width.chars().all(|c| c.is_ascii_digit()),
                "`{func}` asks `alloc_object` for the LITERAL width `{width}`. \
                 Both allocation arms of this function must pass the width \
                 derived from `{derivation}` — the `Err(_)` arm included, because \
                 that is the arm whose row the ratchet counts. A literal restores \
                 the in-bounds write of the wrong field that \
                 W7-68-live-under-allocations.md §3.1 and \
                 W7-72-ssc-socket-and-filechannel.md §2.2 repaired, and no other \
                 gate in this file can see it."
            );
        }
    }
}

/// A RATCHET, and the only one in this file: the `ClassId::new(0)` fallback
/// population may shrink and may not grow.
///
/// [`census`] deliberately refuses to ratchet its count, on the sound ground
/// that a population which changes with every native added gets re-baselined on
/// sight. This population is different in kind: **every member of it is a
/// defect-shaped thing**, so growth is never routine. A native writing
/// `alloc_object(ClassId::new(0), N)` has resolved a class, failed, and gone
/// ahead — handing back an object of class `cratonvm/synthetic/AnonymousObject$N`
/// to a caller that will use it as the class it asked for. **12** of today's 28
/// ask for fewer slots than the class they name really declares (`javap -p`, JDK
/// 25.0.3.9), so those objects are short.
///
/// **The 12 is a re-derivation, and the 14 it replaces was an arithmetic slip
/// worth naming** — the population number and the short number are not the same
/// number and have to be decremented together. W7-74-short-object-repairs.md
/// corrected W7-73's 16 down to 14, then repaired the two `java/lang/Thread`
/// carrier mirrors and took the POPULATION from 30 to 28 — but both repaired
/// sites were members of the 14, so the short count went to 12 at the same
/// moment and was left at 14 here and in that record's §5. Two later, offsetting
/// movements landed on the same day and net to zero, which is exactly how a
/// stale figure gets "confirmed":
///
/// * `native-io/src/lib.rs`'s `native_fc_open` LEAVES the short column.
///   W7-72-ssc-socket-and-filechannel.md moved the `FileChannel` private map
///   onto the appended-slot idiom, so the requested width is now
///   `synthetic_file_channel::alloc_slots` = `base_for_class(…) + 2`. That makes
///   the row structurally unreachable in the same way W7-74 §1.3 established for
///   `MappedByteBuffer`: on an image where `FileChannel` really declares 4 the
///   base is 4 and the request is 6 (an `over` row), and on an image where the
///   request is 2 the base is 0 — meaning the class did not resolve, so there is
///   no declared 4 to be short against.
/// * `native-io/src/socket_channel.rs`'s `alloc_obj` JOINS it. W7-66-live-over-allocations.md
///   §4.3 narrowed its four callers from 12 to `SC_OBJECT_SLOTS` = 6 against
///   `SocketChannel`/`ServerSocketChannel`, which really declare 10 — so the row
///   W7-73 §3.2 filed as "12 against 10 — over" is now 6 against 10, short by 4.
///   A narrowing repair aimed at the `over` census moved a site into the `under`
///   one, and no record noticed on the day.
///
/// The count is prose, not a computation: the declared widths come from `javap`
/// and no Rust test can derive them. Treat it as a work queue, re-derive it
/// after any repair, and decrement BOTH numbers when a short site leaves.
///
/// The number is a source-level count of a shape, not a runtime measurement, and
/// it does not claim any of these arms is ever taken. That is the point: nothing
/// can claim that from source, which is why the runtime row exists as well.
///
/// **How it fails.** Add one more `alloc_object(ClassId::new(0), 3)` to any
/// native crate and this goes red with the new total. (Verified red against a
/// mutated copy with one added site: reported 31 against the then-bound of 30.)
/// Lowering the bound after a repair is the intended edit and needs no
/// discussion; raising it needs a reason in the commit message.
///
/// The scanner was independently replicated a third time on 2026-08-12 — a
/// line-for-line reimplementation of `strip_comments`, `match_brace`,
/// `cfg_test_spans`, `paren_args` and `split_top_level` — and returns the same
/// 28 sites against this tree.
#[test]
fn the_unresolved_class_fallback_population_only_shrinks() {
    /// 2026-08-12, W7-73-short-object-blind-spot.md. Sites whose class argument
    /// is literally `ClassId::new(0)` and whose requested count is not the
    /// literal `0` (a zero-slot request substitutes nothing and asserts no
    /// layout). MAY ONLY GO DOWN.
    ///
    /// 30 → **28** on 2026-08-12 by W7-74-short-object-repairs.md: the two
    /// `java/lang/Thread` carrier mirrors (`vertx_eventloop.rs`,
    /// `xnio_io_thread.rs`) were the only two members of the population that
    /// were **not** on an `Err(_)` fallback arm — they allocated a five-slot
    /// `AnonymousObject$5` unconditionally, on every image, and published it to
    /// the thread registry as a `java.lang.Thread`. Both now go through
    /// `try_alloc_concurrent_synthetic`, which resolves the class and widens to
    /// its declared 19. This is the ratchet doing what its own failure message
    /// prescribes — *"allocate against a class you actually resolved"*.
    ///
    /// **28 re-derived by an independent scanner on 2026-08-12 (later pass) and
    /// unchanged.** The SHORT subset of it is not 14 but **12** — see this
    /// test's doc comment for the arithmetic and for the two offsetting
    /// reclassifications (`native_fc_open` out, `socket_channel::alloc_obj` in)
    /// that make a stale 14 look like it still reconciles.
    const BOUND: usize = 28;

    let root = workspace_root();
    let mut sites: Vec<String> = Vec::new();
    for crate_dir in NATIVE_CRATES {
        for file in rust_sources(&root.join(crate_dir).join("src")) {
            let src = strip_comments(&fs::read_to_string(&file).unwrap_or_default());
            let test_spans = cfg_test_spans(&src);
            for method in LAYOUT_ASSERTING_METHODS {
                let needle = format!("{method}(");
                for (idx, _) in src.match_indices(&needle) {
                    if src[..idx].trim_end().ends_with("fn") {
                        continue;
                    }
                    let prev = src[..idx].chars().next_back().unwrap_or(' ');
                    if prev.is_alphanumeric() || prev == '_' {
                        continue;
                    }
                    if test_spans.iter().any(|(a, b)| *a <= idx && idx < *b) {
                        continue;
                    }
                    let open = idx + needle.len() - 1;
                    let Some(args) = paren_args(&src, open) else {
                        continue;
                    };
                    let parts = split_top_level(args);
                    if parts.is_empty() || !parts[0].contains("ClassId::new(0)") {
                        continue;
                    }
                    let requested = parts.get(1).map_or("", |s| s.as_str()).trim();
                    if requested == "0" {
                        continue;
                    }
                    let line = src[..idx].matches('\n').count() + 1;
                    sites.push(format!(
                        "{}:{line}  requested={requested}",
                        file.strip_prefix(&root)
                            .unwrap_or(&file)
                            .display()
                            .to_string()
                            .replace('\\', "/")
                    ));
                }
            }
        }
    }
    sites.sort();
    println!("layout-alias blind spot — `alloc_object(ClassId::new(0), N>0)` sites");
    for s in &sites {
        println!("  {s}");
    }
    println!("  TOTAL {} (bound {BOUND})", sites.len());
    assert!(
        sites.len() <= BOUND,
        "the `ClassId::new(0)` allocation population GREW to {} (bound {BOUND}):\n  {}\n\
         Each of these resolves a class, fails, and allocates anyway. The VM \
         substitutes `cratonvm/synthetic/AnonymousObject$N` — which declares \
         exactly N, so the slot-count clamp is a no-op and the width census sees \
         agreement — and the object is then handed back as an instance of the class \
         the caller named. Where that class's real layout is wider (12 of the 28 \
         sites after 2026-08-12: `ZipEntry` 6 against 14, `Pattern` 2 against 20, \
         `ServiceLoader` 2 against 10, `Iocp` 1 against 14), the object is SHORT \
         and every real-bytecode read past slot N is out of bounds.\n\
         The remedies that do not add a row here: propagate the resolution failure \
         to the caller (`MethodCallFailed`), or allocate against a class you \
         actually resolved — which is what the two `java/lang/Thread` carrier \
         mirrors did on 2026-08-12 to take the population from 30 to 28.\n\
         If a new site is genuinely unavoidable, raise BOUND with the reason in \
         the commit message.\n\
         See W7-73-short-object-blind-spot.md and W7-74-short-object-repairs.md.",
        sites.len(),
        sites.join("\n  ")
    );
}

/// The argument list of a call whose opening `(` is at `open`.
fn paren_args(src: &str, open: usize) -> Option<&str> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (i, ch) in src[open..].char_indices() {
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
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&src[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split an argument list on top-level commas.
fn split_top_level(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for ch in args.chars() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
        if ch == ',' && depth == 0 {
            out.push(cur.trim().to_string());
            cur = String::new();
        } else {
            cur.push(ch);
        }
    }
    out.push(cur.trim().to_string());
    out
}

// ---------------------------------------------------------------------------
// The census itself — printed, not asserted
// ---------------------------------------------------------------------------

/// Print the direct-allocation population the base observation now covers,
/// split production / `#[cfg(test)]`, so the number in the record can be
/// re-derived by running one test instead of re-reading three crates.
///
/// Deliberately not an assertion. A count ratchet on a population that changes
/// with every native added trains people to re-baseline the file, and a gate
/// people re-baseline on sight protects nothing.
#[test]
fn census() {
    let root = workspace_root();
    let mut prod = 0usize;
    let mut test = 0usize;
    let mut per_crate: Vec<(String, usize, usize)> = Vec::new();
    for crate_dir in NATIVE_CRATES {
        let (mut p, mut t) = (0usize, 0usize);
        for file in rust_sources(&root.join(crate_dir).join("src")) {
            let src = strip_comments(&fs::read_to_string(&file).unwrap_or_default());
            let test_spans = cfg_test_spans(&src);
            for method in LAYOUT_ASSERTING_METHODS {
                let needle = format!("{method}(");
                for (idx, _) in src.match_indices(&needle) {
                    // Skip the declarations themselves.
                    if src[..idx].trim_end().ends_with("fn") {
                        continue;
                    }
                    // Skip `try_alloc_object_gc_safe` matching inside
                    // `alloc_object` scans and vice versa: exact word boundary.
                    let prev = src[..idx].chars().next_back().unwrap_or(' ');
                    if prev.is_alphanumeric() || prev == '_' {
                        continue;
                    }
                    if test_spans.iter().any(|(a, b)| *a <= idx && idx < *b) {
                        t += 1;
                    } else {
                        p += 1;
                    }
                }
            }
        }
        prod += p;
        test += t;
        if p + t > 0 {
            per_crate.push(((*crate_dir).to_string(), p, t));
        }
    }
    println!("layout-alias census — direct NativeContext allocation sites");
    for (name, p, t) in &per_crate {
        println!("  {name:28} production {p:4}   cfg(test) {t:4}");
    }
    println!("  {:28} production {prod:4}   cfg(test) {test:4}", "TOTAL");
    println!(
        "  every production site above now reaches \
         cratonvm_native_api::layout_alias::observe via NativeContextImpl::alloc_object"
    );
    assert!(
        prod > 0,
        "no direct allocation sites found at all — the scanner broke, not the tree"
    );
}

/// Byte spans of `#[cfg(test)]` items, so the census can separate the sites that
/// run against `MockNativeContext` (which never reaches the VM, and so is
/// genuinely outside this census) from the ones that run against a heap.
fn cfg_test_spans(src: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for (idx, _) in src.match_indices("#[cfg(test)]") {
        let Some(open_rel) = src[idx..].find('{') else {
            continue;
        };
        let open = idx + open_rel;
        // An item whose brace never closes extends to end of file. Dropping it
        // instead — which the first draft did — silently reclassifies every
        // allocation inside it as production.
        let close = match_brace(src, open).map_or(src.len(), |c| c + 1);
        spans.push((idx, close));
    }
    spans
}
