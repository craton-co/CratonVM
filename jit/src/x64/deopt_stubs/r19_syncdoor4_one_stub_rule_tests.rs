use super::{
    self_lock_handler_stub_admitted, self_lock_stub_admitted, self_lock_stub_verdict,
    SelfLockStubFacts,
};
use crate::x64::op_invoke::{POST_CALL_OWN_POINT_STUB_TAG, SPLICE_POST_CALL_STUB_TAG};

/// The handlers check's per-stub `match` as it stood at `113aeb71f5`.
fn frozen_handlers_rule(
    reason: i64,
    protected: bool,
    has_site: bool,
    survives: bool,
    hands_over: bool,
    mode_exit_hands_over: bool,
) -> bool {
    match reason {
        3 => !protected || has_site,
        2 | 6 => hands_over,
        9 => protected && has_site && survives,
        10..=12 => protected && has_site,
        7 | POST_CALL_OWN_POINT_STUB_TAG => mode_exit_hands_over,
        _ => false,
    }
}

/// The table-free check's per-stub `match` as it stood at `113aeb71f5`,
/// with each arm's point question answered by `guard` (2 / 6) or `mode`
/// (7 / 13).
fn frozen_table_free_rule(reason: i64, guard: bool, mode: bool) -> bool {
    match reason {
        3 => true,
        2 | 6 => guard,
        7 | POST_CALL_OWN_POINT_STUB_TAG => mode,
        _ => false,
    }
}

fn facts(bits: u32) -> SelfLockStubFacts {
    SelfLockStubFacts {
        protected: bits & 1 != 0,
        has_site: bits & 2 != 0,
        survives: bits & 4 != 0,
        guard_hands_over: bits & 8 != 0,
        mode_exit_hands_over: bits & 16 != 0,
    }
}

#[test]
fn the_one_rule_is_the_handlers_rule_byte_for_byte() {
    for reason in -2..=20i64 {
        for bits in 0..32 {
            let f = facts(bits);
            let old = frozen_handlers_rule(
                reason,
                f.protected,
                f.has_site,
                f.survives,
                f.guard_hands_over,
                f.mode_exit_hands_over,
            );
            assert_eq!(
                self_lock_stub_admitted(reason, f),
                old,
                "reason {reason} {f:?}"
            );
            assert_eq!(
                self_lock_handler_stub_admitted(
                    reason,
                    f.protected,
                    f.has_site,
                    f.survives,
                    f.guard_hands_over,
                    f.mode_exit_hands_over,
                ),
                old,
                "reason {reason} {f:?}"
            );
        }
    }
}

#[test]
fn the_one_rule_is_the_table_free_rule_byte_for_byte() {
    for reason in -2..=20i64 {
        for (guard, mode) in [(false, false), (true, false), (false, true), (true, true)] {
            // As the table-free check asks it: the guard answer only for
            // 2 / 6 (`matches!(reason, 2 | 6) && ..`), the mode answer as
            // `self_lock_mode_exit_stub_hands_over` gives it (`false` for
            // any reason but 7 / 13).
            let f = SelfLockStubFacts {
                guard_hands_over: matches!(reason, 2 | 6) && guard,
                mode_exit_hands_over: matches!(reason, 7 | POST_CALL_OWN_POINT_STUB_TAG)
                    && mode,
                ..SelfLockStubFacts::TABLE_FREE
            };
            assert_eq!(
                self_lock_stub_admitted(reason, f),
                frozen_table_free_rule(reason, guard, mode),
                "reason {reason} guard {guard} mode {mode}"
            );
        }
    }
}

/// Rustfmt may split the receiver from `.push` and add a trailing tuple
/// comma. Match tokens without whitespace and balance argument delimiters
/// so a nested expression cannot end the scan before the tag.
fn emitted_constant_stub_tags(text: &str) -> Vec<i64> {
    let compact: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let needle = "deopt_stubs.push((";
    let mut rest = compact.as_str();
    let mut tags = Vec::new();
    while let Some(at) = rest.find(needle) {
        let tail = &rest[at + needle.len()..];
        let mut depth = 1usize;
        let mut start = 0usize;
        let mut last = "";
        let mut end = None;
        for (i, byte) in tail.bytes().enumerate() {
            match byte {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        if start < i {
                            last = &tail[start..i];
                        }
                        end = Some(i);
                        break;
                    }
                }
                b',' if depth == 1 => {
                    if start < i {
                        last = &tail[start..i];
                    }
                    start = i + 1;
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            break;
        };
        let token = last.rsplit("::").next().unwrap_or(last);
        let value = match token {
            "POST_CALL_OWN_POINT_STUB_TAG" => Some(POST_CALL_OWN_POINT_STUB_TAG),
            "SPLICE_POST_CALL_STUB_TAG" => Some(SPLICE_POST_CALL_STUB_TAG),
            literal => literal.parse::<i64>().ok(),
        };
        if let Some(value) = value {
            tags.push(value);
        }
        rest = &tail[end + 1..];
    }
    tags
}

#[test]
fn stub_tag_scan_survives_formatting_nested_arguments_and_unknown_tags() {
    let receiver = "deopt_stubs";
    for source in [
        format!("self.{receiver}.push((site((1,2)),index,{SPLICE_POST_CALL_STUB_TAG}));"),
        format!("self.{receiver}\n .push((site((1,2)), index, crate::x64::op_invoke::SPLICE_POST_CALL_STUB_TAG, ));"),
    ] {
        assert_eq!(emitted_constant_stub_tags(&source), vec![SPLICE_POST_CALL_STUB_TAG]);
    }
    let unknown = 99;
    let source = format!("self.{receiver}\n .push((site((1,2)), index, {unknown}, ));");
    assert_eq!(emitted_constant_stub_tags(&source), vec![unknown]);
    assert!(self_lock_stub_verdict(unknown, SelfLockStubFacts::TABLE_FREE).is_none());
}

/// Every `deopt_stubs.push((.., TAG))` the single-pass emitter writes
/// with a literal or named tag is a tag the one rule NAMES (admits or
/// refuses on purpose), so a new stub reason cannot reach a self-locking
/// body's check unconsidered. Tag 0 is the test-only injection
/// (`inlining.rs` `force_inline_deopt_publication`).
#[test]
fn every_emitted_stub_tag_is_named_by_the_one_rule() {
    let sources: [(&str, &str); 9] = [
        ("arith.rs", include_str!("../arith.rs")),
        ("arrays.rs", include_str!("../arrays.rs")),
        ("bytecode_walk.rs", include_str!("../bytecode_walk.rs")),
        ("deopt_stubs.rs", include_str!("../deopt_stubs.rs")),
        (
            "inlining.rs",
            concat!(
                include_str!("../inlining.rs"),
                "\n",
                include_str!("../inlining/spliced_sites.rs")
            ),
        ),
        ("op_control.rs", include_str!("../op_control.rs")),
        (
            "op_invoke.rs",
            concat!(
                include_str!("../op_invoke/mod.rs"),
                "\n",
                include_str!("../op_invoke/sp_tailcall_keeps_vm_helper_frame.rs"),
                "\n",
                include_str!("../op_invoke/sp_byte_array_view_body.rs"),
                "\n",
                include_str!("../op_invoke/walk_invoke_instance.rs"),
                "\n",
                include_str!("../op_invoke/arraylist_access_prefix.rs"),
                "\n",
                include_str!("../op_invoke/indy_site_is_bridged.rs"),
                "\n",
                include_str!("../op_invoke/sb_to_string_latin1_body.rs")
            ),
        ),
        ("osr.rs", include_str!("../osr.rs")),
        ("safepoint.rs", include_str!("../safepoint.rs")),
    ];
    let mut seen = std::collections::BTreeSet::new();
    for (file, text) in sources {
        for value in emitted_constant_stub_tags(text) {
            if value != 0 {
                assert!(
                    self_lock_stub_verdict(value, SelfLockStubFacts::TABLE_FREE).is_some(),
                    "{file}: stub tag {value} is not named by `self_lock_stub_verdict`"
                );
            }
            seen.insert(value);
        }
    }
    // Positive control: the scan found the tags it must find.
    for must in [2, 3, 6, 7, 8, 9, 10, 11, 12, SPLICE_POST_CALL_STUB_TAG] {
        assert!(seen.contains(&must), "scan missed tag {must}: {seen:?}");
    }
}
