// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 21, lane str21b (STRINGS2-21-4): the default-on
//! `StringLatin1.regionMatchesCI` prefix's naming, switch and splice-planner
//! facts. The machine code itself is executed by
//! `x64::latin1_region_ci_emit::str21b_region_matches_ci_emit_tests`.

use super::{
    ir_splice_nested_case_prefix_stays_call, ir_string_prefix_site_stays_a_call,
    ir_string_search_prefix_kind, IrStringSearchKind as K,
};
use cratonvm_types::flags::with_thread_overrides;

const L1: &str = "java/lang/StringLatin1";
const RM: &str = "([BI[BII)Z";
const KEY: &str = "CRATONVM_JIT_IR_LATIN1_REGION_MATCHES_CI_PREFIX";
const SEARCH: &str = "CRATONVM_JIT_IR_STRING_SEARCH_PREFIX";

fn named(class: &str, name: &str, desc: &str, edits: &[(&str, Option<&str>)]) -> Option<K> {
    with_thread_overrides(edits, || ir_string_search_prefix_kind(class, name, desc))
}

/// The site is named under its own default-on switch (and the String search
/// prefix's master switch), and only for the one JDK member.
#[test]
fn str21b_region_matches_ci_defaults_on_and_respects_its_switch() {
    assert_eq!(
        named(L1, "regionMatchesCI", RM, &[(KEY, None)]),
        Some(K::Latin1RegionMatchesCI),
        "unset uses the enabled default"
    );
    assert_eq!(named(L1, "regionMatchesCI", RM, &[(KEY, Some("0"))]), None);
    assert_eq!(
        named(L1, "regionMatchesCI", RM, &[(KEY, Some("1"))]),
        Some(K::Latin1RegionMatchesCI)
    );
    assert_eq!(
        named(
            L1,
            "regionMatchesCI",
            RM,
            &[(KEY, Some("1")), (SEARCH, Some("0"))]
        ),
        None,
        "the master switch withdraws it"
    );
    let on = [(KEY, Some("1"))];
    // The UTF16 / mixed workers, other descriptors and other classes are not it.
    assert_eq!(named(L1, "regionMatchesCI_UTF16", RM, &on), None);
    assert_eq!(named(L1, "regionMatchesCI", "([BI[BI)Z", &on), None);
    assert_eq!(named(L1, "regionMatchesCI", "([BI[BII)I", &on), None);
    assert_eq!(
        named("java/lang/StringUTF16", "regionMatchesCI", RM, &on),
        None
    );
    assert_eq!(named("java/lang/String", "regionMatchesCI", RM, &on), None);
    // Its neighbours are unaffected by the new arm.
    assert_eq!(
        named(L1, "compareToCI", "([B[B)I", &[(KEY, Some("1"))]),
        named(L1, "compareToCI", "([B[B)I", &[(KEY, None)]),
    );
}

/// The kind's static facts: a static (opcode `invokestatic`) site with the
/// array standing where a receiver would and four parameters behind it, not a
/// `toLowerCase` kind (its own emitter), and kept a call by the splice planner.
#[test]
fn str21b_region_matches_ci_kind_facts() {
    let k = K::Latin1RegionMatchesCI;
    assert_eq!(
        k.num_params(),
        4,
        "toffset, other, ooffset, len behind `value`"
    );
    for invoke_kind in 0..=5u8 {
        assert_eq!(
            k.admits_invoke_kind(invoke_kind),
            invoke_kind == 3,
            "{invoke_kind}"
        );
    }
    for opcode in [0xb6u8, 0xb7, 0xb8, 0xb9] {
        assert_eq!(k.admits_opcode(opcode), opcode == 0xb8, "{opcode:#x}");
    }
    assert!(!k.is_latin1_case() && !k.is_builder() && !k.is_chars_copy() && !k.is_string_coding());
    assert!(
        ir_string_prefix_site_stays_a_call(k),
        "spliced, it is the JDK per-byte loop"
    );
}

/// A `regionMatchesCI` call nested in a spliced `String.regionMatches` is kept
/// a bound call (the nested case-prefix drop), but only when the prefix itself
/// is on, including its enabled default.
#[test]
fn str21b_nested_region_matches_ci_keeps_its_call_only_under_the_switch() {
    if crate::x64::narrow_oops_block_inline_fields()
        || cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_IR_STRING_ACCESS_ADMIT")
    {
        return;
    }
    let stays = |edits: &[(&str, Option<&str>)]| {
        with_thread_overrides(edits, || {
            ir_splice_nested_case_prefix_stays_call(L1, "regionMatchesCI", RM, 3)
        })
    };
    assert!(stays(&[(KEY, None)]), "the default enables the prefix");
    assert!(!stays(&[(KEY, Some("0"))]), "the prefix kill also withdraws the splice carve-out");
    assert!(stays(&[(KEY, Some("1"))]));
    assert!(
        !stays(&[
            (KEY, Some("1")),
            ("CRATONVM_JIT_IR_LATIN1_CASE_NESTED_TO_CALL", Some("0"))
        ]),
        "the nested-to-call switch withdraws it"
    );
    let not_static = with_thread_overrides(&[(KEY, Some("1"))], || {
        ir_splice_nested_case_prefix_stays_call(L1, "regionMatchesCI", RM, 0)
    });
    assert!(!not_static, "only an invokestatic site");
}
