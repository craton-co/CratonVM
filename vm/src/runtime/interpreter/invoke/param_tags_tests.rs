use super::{nth_param_tag_byte, ParamTags};

/// The whole safety argument for replacing the per-argument
/// `nth_param_tag_byte` scan with one `ParamTags::of`: the two must answer
/// identically for EVERY index, including out-of-range ones, or a
/// category-2 `long`/`double` argument gets popped down the category-1
/// path and its high bits are silently dropped. That is the exact failure
/// `nth_param_tag_byte`'s own call sites were written to prevent (BC
/// safegcd `0xFFFC_…` accumulators), and it is silent — a wrong tag
/// produces a plausible number, not a crash.
///
/// Indices are probed past the parameter count on purpose: the dispatch
/// arms index by argument slot, which for a wide (category-2) descriptor
/// runs past the parameter count.
#[test]
fn param_tags_match_nth_param_tag_byte() {
    let mut descriptors: Vec<String> = vec![
        "()V".to_string(),
        "()I".to_string(),
        "(I)I".to_string(),
        "(J)J".to_string(),
        "(D)D".to_string(),
        "(F)V".to_string(),
        "(Z)Z".to_string(),
        "(B)B".to_string(),
        "(S)S".to_string(),
        "(C)C".to_string(),
        "(Ljava/lang/String;)V".to_string(),
        "([I)V".to_string(),
        "([[Ljava/lang/Object;)V".to_string(),
        "(IJDLjava/lang/String;[BF)Ljava/lang/Object;".to_string(),
        "(Ljava/lang/String;Ljava/lang/String;)Z".to_string(),
        "([Ljava/lang/String;[[JI)V".to_string(),
        // Degenerate/malformed shapes the scanner must not disagree on.
        "(".to_string(),
        "()".to_string(),
        "(L".to_string(),
        "([".to_string(),
        "(Ljava/lang/String".to_string(),
    ];

    // Exactly at, one below and one above the inline capacity, so the
    // overflow fallback is exercised rather than assumed.
    for n in [15usize, 16, 17, 40] {
        descriptors.push(format!("({})V", "I".repeat(n)));
        descriptors.push(format!("({})V", "J".repeat(n)));
        descriptors.push(format!("({})V", "Ljava/lang/String;".repeat(n)));
        descriptors.push(format!("({})V", "[I".repeat(n)));
    }

    // Both settings of `VmConfig::param_tag_scan`: the scan, and the
    // kill switch's per-index rescan.
    for scan in [true, false] {
        for d in &descriptors {
            let tags = ParamTags::of(d, scan);
            for n in 0..64 {
                assert_eq!(
                    tags.get(d, n),
                    nth_param_tag_byte(d, n),
                    "descriptor {d:?} index {n} scan {scan}"
                );
            }
        }
    }

    // The memoized tokenisation must answer identically to the per-call
    // one, on every descriptor above including the malformed and the
    // overflowing. `DescriptorFacts::of` is this scan moved to where it
    // can be cached on `CachedBytecodeMethod`; if the two ever drift, the
    // invoke path decodes an argument against the wrong tag, which is a
    // silent wrong value rather than a crash — exactly the failure mode
    // `nth_param_tag_byte`'s own call sites exist to prevent.
    for d in &descriptors {
        let facts = cratonvm_jit_api::DescriptorFacts::of(d);
        let from_facts = ParamTags::from_facts(&facts, true);
        let scanned = ParamTags::of(d, true);
        for n in 0..64 {
            assert_eq!(
                from_facts.get(d, n),
                scanned.get(d, n),
                "DescriptorFacts disagrees with ParamTags::of for {d:?} index {n}"
            );
        }
        // And the return tag against the scan it replaced.
        assert_eq!(
            facts.ret_tag,
            crate::jit::return_type(d),
            "DescriptorFacts::ret_tag disagrees with jit::return_type for {d:?}"
        );
        // The claim the cached-native arms rest on, stated directly:
        // when the tag array is complete, reading `param_tags[i]` is the
        // same byte `pop_coerced_invoke_args_*` would have obtained by
        // scanning the descriptor it re-resolved. `native_site_facts_usable`
        // admits exactly this shape.
        if !facts.param_tags_overflow {
            for i in 0..facts.param_tag_len as usize {
                assert_eq!(
                    facts.param_tags[i],
                    scanned.get(d, i),
                    "cached-native facts tag {i} disagrees with the scan for {d:?}"
                );
            }
        }
    }
}

/// `native_site_facts_usable` must refuse exactly the two shapes the tag
/// array cannot describe, and admit everything else.
///
/// It is the whole guard between the facts-driven argument pop and the
/// general helper, and both of its refusals are silent-wrong-answer shapes
/// rather than crashes: an overflowing descriptor has no tags past the
/// eighth parameter, and a `num_params` that disagrees with the tokenised
/// count means the entry and the descriptor describe different methods.
#[test]
fn cached_native_facts_are_refused_for_the_shapes_they_cannot_describe() {
    use crate::runtime::interpreter::native_site_facts_usable;
    let ok = cratonvm_jit_api::DescriptorFacts::of("(IJLjava/lang/String;)V");
    assert!(native_site_facts_usable(&ok, 3, false));
    assert!(native_site_facts_usable(&ok, 3, true));
    // A count that disagrees with the tokenised one.
    assert!(!native_site_facts_usable(&ok, 2, false));
    assert!(!native_site_facts_usable(&ok, 4, false));
    // More parameters than the inline tag array holds.
    let over = cratonvm_jit_api::DescriptorFacts::of("(IIIIIIIII)V");
    assert!(over.param_tags_overflow);
    assert!(!native_site_facts_usable(&over, 9, false));
    // Zero parameters is the commonest cached-native shape of all.
    let none = cratonvm_jit_api::DescriptorFacts::of("()I");
    assert!(native_site_facts_usable(&none, 0, true));
}

/// Slot 0 of a non-static call is the receiver and must answer `b'L'`
/// whatever the descriptor says, with parameter `k` at slot `k + 1`.
#[test]
fn get_with_receiver_offsets_by_one() {
    let d = "(JLjava/lang/String;I)V";
    let tags = ParamTags::of(d, true);
    assert_eq!(tags.get_with_receiver(d, 0), b'L');
    for k in 0..8 {
        assert_eq!(
            tags.get_with_receiver(d, k + 1),
            nth_param_tag_byte(d, k),
            "slot {} vs param {k}",
            k + 1
        );
    }
}
