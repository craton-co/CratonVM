// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! OWN-BODY VIRTUAL FALLBACK RATCHET (JIT round 21 lane reflect `eefa6b0c3e`;
//! DEPTH21-3; re-landed t1 wave 8 lane RL-REFL onto the decomposed
//! `src/lang_invoke/` module after merge `624fae0b49` dropped it; audit in
//! `docs/internal/fixed-bugs/jr21-reflect-own-body-virtual-fallbacks-FIXED-20261015.md`).
//!
//! A native registered on a `java/lang/invoke/MethodHandle` method that falls
//! back to "the JDK's own body" with `invoke_virtual_bytecode_only(this,
//! "<its own name>", ..)` re-selects on the receiver's class. Reached through
//! a subclass override's `super.m()` (an `invokespecial` of the very triple
//! the native is registered on), that selection lands on the override again
//! and the two call each other until the stack runs out: interpreter round i4
//! wave 84's `L4W84` row (`AsVarargsCollector.invokeWithArguments` ->
//! `super.invokeWithArguments`), fixed with `invoke_special_bytecode_only`;
//! round 21 (lane reflect) fixed the adapter natives the same way
//! (`MethodHandleImpl$IntrinsicMethodHandle.asCollector` ->
//! `super.asCollector`).
//!
//! So every such call is reviewed: the count of
//! `.invoke_virtual_bytecode_only(` per file of `src/lang_invoke/` is EXACT.
//! The baseline is the count at the t1 wave 8 re-land (dev had grown the
//! round 21 audit's 17 to 21 sites; the re-land turned the adapter natives'
//! site into an `invokespecial`, leaving 20; dev's jr21 re-land made both
//! `invokeWithArguments` fallbacks in `adapters.rs` unconditional
//! `invokespecial`s, leaving 18 at the t1 wave 13 dev merge, and pointed its
//! own copy of this file at a `src/lang_invoke.rs` that no longer exists, so
//! the per-file list here stands). A new site fails until it is
//! checked against its receiver classes' overrides; a removed one fails until
//! the baseline is lowered. Comments that quote a call are counted,
//! deterministically.

use std::path::Path;

/// `(file relative to this crate's manifest, exact count)`.
const BASELINE: &[(&str, usize)] = &[
    ("src/lang_invoke/adapters.rs", 7),
    ("src/lang_invoke/call_site.rs", 1),
    ("src/lang_invoke/combinators.rs", 0),
    ("src/lang_invoke/direct_dispatch.rs", 0),
    ("src/lang_invoke/member_name.rs", 6),
    ("src/lang_invoke/var_handles.rs", 4),
];

const NEEDLE: &str = ".invoke_virtual_bytecode_only(";

#[test]
fn own_body_virtual_fallbacks_are_reviewed() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut failures = Vec::new();
    for (rel, expected) in BASELINE {
        let path = manifest.join(rel);
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let total = text.matches(NEEDLE).count();
        if total != *expected {
            failures.push(format!(
                "{rel}: {total} `{NEEDLE}` calls, baseline {expected}. {}",
                if total > *expected {
                    "GREW: review the new site against \
                     jr21-reflect-own-body-virtual-fallbacks-FIXED-20261015.md (a native's fallback to \
                     its OWN triple must not re-select an override that calls super; use \
                     `invoke_special_bytecode_only` where only the declaring class's body is \
                     meant), add it there, then raise the baseline."
                } else {
                    "SHRANK: lower the baseline in this file to the number above."
                }
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The adapter natives' own-body fallback runs `MethodHandle`'s body by
/// `invokespecial` (the round 21 fix itself).
#[test]
fn the_adapter_fallback_is_an_invokespecial_of_method_handle() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(manifest.join("src/lang_invoke/member_name.rs"))
        .expect("member_name.rs");
    let start = text
        .find("pub(crate) fn mh_foreign_adapter_through_jdk_cold(")
        .expect("the adapter fallback");
    let body = &text[start..];
    let body = &body[..body.find("\n}\n").expect("its end")];
    assert!(body.contains(
        "ctx.invoke_special_bytecode_only(\"java/lang/invoke/MethodHandle\", name, desc, &call)"
    ));
    assert!(!body.contains(NEEDLE));
}
