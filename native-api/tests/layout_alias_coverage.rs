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
        } else if path.extension().is_some_and(|e| e == "rs") {
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
        let sig_end = registry[fn_at..].find('{').unwrap_or(registry.len() - fn_at) + fn_at;
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
    let body = fn_body(&strip_comments(&src), "alloc_object")
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
    assert!(prod > 0, "no direct allocation sites found at all — the scanner broke, not the tree");
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
