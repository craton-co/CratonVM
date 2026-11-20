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

fn named(class: &str, name: &str, desc: &str, edits: &[(&str, Option<&str>)]) -> Option<K> {
    with_thread_overrides(edits, || ir_string_search_prefix_kind(class, name, desc))
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

// A `regionMatchesCI` call nested in a spliced `String.regionMatches` is kept
// a bound call (the nested case-prefix drop), but only when the prefix itself
// is on, including its enabled default.

