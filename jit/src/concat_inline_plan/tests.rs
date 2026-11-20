use super::*;

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[test]
fn an_int_and_string_recipe_is_split_in_order() {
    let recipe = units("v=\u{1}/\u{1}");
    let plan = JitConcatInlinePlan::from_recipe(
        &recipe,
        &[],
        "(ILjava/lang/String;)Ljava/lang/String;",
    )
    .expect("admitted");
    assert_eq!(
        plan.segments,
        vec![
            ConcatSegment::Literal(b"v=".to_vec()),
            ConcatSegment::Arg {
                index: 0,
                kind: ConcatArgKind::Int
            },
            ConcatSegment::Literal(b"/".to_vec()),
            ConcatSegment::Arg {
                index: 1,
                kind: ConcatArgKind::String
            },
        ]
    );
    assert_eq!(plan.literal_len, 3);
    assert_eq!(plan.arg_count, 2);
}

#[test]
fn constants_merge_into_the_neighbouring_literal() {
    let recipe = units("<\u{2}|\u{1}\u{2}>");
    let k = units("K");
    let q = units("\u{e9}");
    let plan = JitConcatInlinePlan::from_recipe(
        &recipe,
        &[k.as_slice(), q.as_slice()],
        "(J)Ljava/lang/String;",
    )
    .expect("admitted");
    assert_eq!(
        plan.segments,
        vec![
            ConcatSegment::Literal(b"<K|".to_vec()),
            ConcatSegment::Arg {
                index: 0,
                kind: ConcatArgKind::Long
            },
            ConcatSegment::Literal(vec![0xE9, b'>']),
        ]
    );
    assert_eq!(plan.literal_len, 5);
}

#[test]
fn every_admitted_kind_maps_from_its_descriptor() {
    let recipe = units("\u{1}\u{1}\u{1}\u{1}\u{1}\u{1}\u{1}");
    let plan = JitConcatInlinePlan::from_recipe(
        &recipe,
        &[],
        "(ZBCSIJLjava/lang/String;)Ljava/lang/String;",
    )
    .expect("admitted");
    let kinds: Vec<ConcatArgKind> = plan
        .segments
        .iter()
        .filter_map(|s| match s {
            ConcatSegment::Arg { kind, .. } => Some(*kind),
            ConcatSegment::Literal(_) | ConcatSegment::WideLiteral(_) => None,
        })
        .collect();
    use ConcatArgKind::*;
    assert_eq!(kinds, vec![Boolean, Int, Char, Int, Int, Long, String]);
    assert_eq!(plan.literal_len, 0);
}

#[test]
fn what_needs_tostring_or_utf16_keeps_the_bridge() {
    let one = units("\u{1}");
    for d in [
        "(Ljava/lang/Object;)Ljava/lang/String;",
        "([I)Ljava/lang/String;",
        "(Ljava/lang/StringBuilder;)Ljava/lang/String;",
    ] {
        assert_eq!(JitConcatInlinePlan::from_recipe(&one, &[], d), None, "{d}");
    }
    // A literal above 0xFF: the result is always UTF16, so the plan is
    // WIDE (STRGAP4-4), which a compiler without CRATONVM_JIT_CONCAT_UTF16
    // refuses -- the bridge call this site always had.
    let p =
        JitConcatInlinePlan::from_recipe(&units("\u{3a9}\u{1}"), &[], "(I)Ljava/lang/String;")
            .expect("wide plan");
    assert!(p.wide);
    let wide = units("\u{3a9}");
    let p = JitConcatInlinePlan::from_recipe(
        &units("\u{2}"),
        &[wide.as_slice()],
        "()Ljava/lang/String;",
    )
    .expect("wide plan");
    assert!(p.wide);
}

/// STRGAP4-4: a wide plan keeps every literal as UTF-16 units (Latin-1
/// text included, merged with the constants), in recipe order, and counts
/// units; a Latin-1 recipe is never wide.
#[test]
fn a_wide_literal_makes_a_wide_plan_of_units() {
    let recipe = units("\u{3a9}=\u{1}<\u{2}>\u{1}");
    let k = units("k\u{e9}");
    let plan = JitConcatInlinePlan::from_recipe(
        &recipe,
        &[k.as_slice()],
        "(CLjava/lang/String;)Ljava/lang/String;",
    )
    .expect("admitted");
    assert!(plan.wide);
    assert_eq!(
        plan.segments,
        vec![
            ConcatSegment::WideLiteral(units("\u{3a9}=")),
            ConcatSegment::Arg {
                index: 0,
                kind: ConcatArgKind::Char
            },
            ConcatSegment::WideLiteral(units("<k\u{e9}>")),
            ConcatSegment::Arg {
                index: 1,
                kind: ConcatArgKind::String
            },
        ]
    );
    assert_eq!(plan.literal_len, 6);
    let narrow =
        JitConcatInlinePlan::from_recipe(&units("\u{e9}\u{1}"), &[], "(I)Ljava/lang/String;")
            .expect("admitted");
    assert!(!narrow.wide);
    assert_eq!(narrow.segments[0], ConcatSegment::Literal(vec![0xE9]));
}

#[test]
fn mismatched_tags_and_oversize_sites_keep_the_bridge() {
    // Two argument tags, one operand.
    assert_eq!(
        JitConcatInlinePlan::from_recipe(&units("\u{1}\u{1}"), &[], "(I)Ljava/lang/String;"),
        None
    );
    // An unused operand.
    assert_eq!(
        JitConcatInlinePlan::from_recipe(&units("\u{1}"), &[], "(II)Ljava/lang/String;"),
        None
    );
    // A constant tag with no constant, and a constant with no tag.
    assert_eq!(
        JitConcatInlinePlan::from_recipe(&units("\u{2}"), &[], "()Ljava/lang/String;"),
        None
    );
    let k = units("k");
    assert_eq!(
        JitConcatInlinePlan::from_recipe(&units("x"), &[k.as_slice()], "()Ljava/lang/String;"),
        None
    );
    // Past the literal bound.
    let long_text = "a".repeat(CONCAT_INLINE_MAX_LITERAL + 1);
    assert_eq!(
        JitConcatInlinePlan::from_recipe(&units(&long_text), &[], "()Ljava/lang/String;"),
        None
    );
    // Past the operand bound.
    let n = CONCAT_INLINE_MAX_ARGS + 1;
    let recipe: Vec<u16> = vec![CONCAT_TAG_ARG; n];
    let desc = format!("({})Ljava/lang/String;", "I".repeat(n));
    assert_eq!(JitConcatInlinePlan::from_recipe(&recipe, &[], &desc), None);
    // A malformed descriptor.
    assert_eq!(
        JitConcatInlinePlan::from_recipe(&units("\u{1}"), &[], "(Q)V"),
        None
    );    }