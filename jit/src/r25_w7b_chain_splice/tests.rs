    use super::*;

    /// `ASCII.getType(I)I` as javac emits it: `iload_0; ldc; iand; ifne
    /// +11; getstatic ctype; iload_0; iaload; goto +4; iconst_0; ireturn`
    /// (17 bytes).
    const GET_TYPE: [u8; 17] = [
        0x1a, 0x12, 0x02, 0x7e, 0x9a, 0x00, 0x0b, 0xb2, 0x00, 0x03, 0x1a, 0x2e, 0xa7, 0x00,
        0x04, 0x03, 0xac,
    ];



    /// The W11-7 review fix: the raise holds inside a chain tree only,
    /// unless `CRATONVM_JIT_IR_NESTED_CHAIN_DEPTH_SCOPED=0`.


    #[test]
    fn r25_w9_3_the_chain_compile_line_names_the_outcome() {
        let mut census = ChainCompileCensus::default();
        census.note(&ChainTreeReport {
            kept: 2,
            costly: 1,
            est_frame_before: 96,
            est_frame_after: 96 + 760,
            min_entry_permille: Some(500),
            ..ChainTreeReport::default()
        });
        census.note(&ChainTreeReport {
            unrooted: 1,
            est_frame_before: 40,
            est_frame_after: 40,
            ..ChainTreeReport::default()
        });
        assert_eq!(
            (
                census.trees,
                census.links,
                census.costly,
                census.est_frame_added
            ),
            (1, 2, 1, 760)
        );
        assert_eq!(census.min_entry_permille, Some(500));
        let rec = ir_evidence::CompileRecord {
            spliced_frames: 5,
            blind_dispatches_in_splice: 1,
            ..ir_evidence::CompileRecord::default()
        };
        let refused = census.line("M.m()V", true, false, Some(1184), Some(rec));
        assert!(refused.starts_with("[cratonvm-jitc] chain-compile M.m()V: trees=1 links=2"));
        assert!(refused.contains("real_frame=1184"));
        assert!(refused.contains("verdict=REFUSED by the acceptance gate"));
        assert!(refused.contains("blind_in_splice=1"));
        assert!(refused.contains(&format!("added_ns={}", rec.added_ns_per_execution())));
        assert!(census
            .line("M.m()V", false, false, None, None)
            .contains("real_frame=- verdict=NOT-LOWERED"));
        assert!(census
            .line("M.m()V", true, true, Some(64), None)
            .contains("verdict=accepted evidence=unjudged"));
    }

    const MATCH_DESC: &str = "(Ljava/util/regex/Matcher;ILjava/lang/CharSequence;)Z";

    fn target(kind: u8, name: &str, desc: &str) -> InlineInvokeTarget {
        InlineInvokeTarget {
            invoke_kind: kind,
            method_name: name.to_string(),
            descriptor: desc.to_string(),
            ..Default::default()
        }
    }

    /// `BmpCharPropertyGreedy.match`'s shape: a predicate call (spliced
    /// here: it has a nested row) and the chain's next link.
    fn greedy_like(len: usize) -> InlineSite {
        let mut site = InlineSite {
            callee_code: vec![0xac; len],
            callee_code_len: len,
            method_name: "match".to_string(),
            descriptor: MATCH_DESC.to_string(),
            ..InlineSite::default()
        };
        site.invoke_targets.push((10, target(2, "is", "(I)Z")));
        site.invoke_targets
            .push((20, target(0, "match", MATCH_DESC)));
        site.nested_sites.push(NestedInlineSite {
            callee_pc: 10,
            guard_class_id: 900,
            site: InlineSite::default(),
            bimorphic_second: None,
        });
        site
    }

    /// `Pattern$Start.match`'s shape: the chain call in a loop.
    fn start_like() -> InlineSite {
        InlineSite {
            callee_code: LOOPING.to_vec(),
            callee_code_len: LOOPING.len(),
            method_name: "match".to_string(),
            descriptor: MATCH_DESC.to_string(),
            invoke_targets: vec![(5, target(0, "match", MATCH_DESC))],
            ..InlineSite::default()
        }
    }

    /// `iload_1; ifeq 11; aload_0; invokevirtual #2; goto 4; return`: the
    /// call at pc 5 is in a loop, behind a branch.
    const LOOPING: [u8; 12] = [
        0x1b, 0x99, 0x00, 0x0a, 0x2a, 0xb6, 0x00, 0x02, 0xa7, 0xff, 0xfc, 0xb1,
    ];
    /// `iload_1; ifeq 8; aload_0; invokevirtual #2; iconst_1; ireturn`:
    /// the call at pc 5 is behind a branch, in no loop.
    const BEHIND: [u8; 10] = [0x1b, 0x99, 0x00, 0x07, 0x2a, 0xb6, 0x00, 0x02, 0x04, 0xac];

    fn samples(n: u32) -> profile::ReceiverCounts {
        let mut counts = profile::ReceiverCounts::default();
        counts.insert(608, n);
        counts
    }

    /// The chain switch as given, and every switch the rules here read
    /// pinned: the nested guarded row and its hot caps on, the two other
    /// over-cold-cap admissions (`_FORWARDER_EXACT_HOT`,
    /// `_PSPLICE_NESTED_EXACT_HOT`) off so only the rule under test admits.

    #[test]
    fn r25_w7b_a_chain_call_is_a_virtual_call_of_the_bodys_own_selector() {
        assert!(is_chain_call(0, "match", MATCH_DESC, "match", MATCH_DESC));
        assert!(is_chain_call(2, "match", MATCH_DESC, "match", MATCH_DESC));
        assert!(!is_chain_call(3, "match", MATCH_DESC, "match", MATCH_DESC));
        assert!(!is_chain_call(1, "match", MATCH_DESC, "match", MATCH_DESC));
        assert!(!is_chain_call(0, "is", "(I)Z", "match", MATCH_DESC));
        assert!(!is_chain_call(0, "match", "(I)Z", "match", MATCH_DESC));
        assert!(body_is_chain_shaped(&greedy_like(89)));
        let mut leaf = greedy_like(45);
        leaf.invoke_targets.clear();
        assert!(!body_is_chain_shaped(&leaf));
        assert_eq!(first_kept_non_link_call(&greedy_like(89)), None);
    }





    #[test]
    fn r25_w7b_fix_an_object_call_of_the_bodys_selector_is_no_chain() {
        let mut element_hash = target(0, "hashCode", "()I");
        element_hash.class_name = "java/lang/Object".to_string();
        assert!(!is_chain_target(&element_hash, "hashCode", "()I"));
        let mut next = target(0, "match", MATCH_DESC);
        next.class_name = "java/util/regex/Pattern$Node".to_string();
        assert!(is_chain_target(&next, "match", MATCH_DESC));
        // An Object-typed same-selector call is a kept call to the link rule.
        let mut set_hash = InlineSite {
            callee_code_len: 60,
            method_name: "hashCode".to_string(),
            descriptor: "()I".to_string(),
            ..InlineSite::default()
        };
        set_hash.invoke_targets.push((7, element_hash));
        assert!(!body_is_chain_shaped(&set_hash));
        assert_eq!(first_kept_non_link_call(&set_hash), Some(7));
    }

    /// `greedy_like` whose next-link call at pc 20 carries a nested row
    /// of `len` bytes (a `LastNode`-like leaf).
    fn greedy_with_next(greedy_len: usize, len: usize) -> InlineSite {
        let mut greedy = greedy_like(greedy_len);
        greedy.nested_sites.push(NestedInlineSite {
            callee_pc: 20,
            guard_class_id: 610,
            site: InlineSite {
                callee_code: vec![0xac; len],
                callee_code_len: len,
                callee_max_locals: 3,
                method_name: "match".to_string(),
                descriptor: MATCH_DESC.to_string(),
                ..InlineSite::default()
            },
            bimorphic_second: None,
        });
        greedy
    }

    fn tree(parent_code: &[u8], link: InlineSite) -> InlineSite {
        let mut tree = start_like();
        tree.callee_code = parent_code.to_vec();
        tree.callee_code_len = parent_code.len();
        tree.nested_sites.push(NestedInlineSite {
            callee_pc: 5,
            guard_class_id: 608,
            site: link,
            bimorphic_second: None,
        });
        tree
    }




    #[test]
    fn r25_w8_5_a_link_must_pay_for_the_frame_it_adds() {
        // Greedy's estimate: 89 bytes + 6 locals = 760 frame bytes.
        assert_eq!(est_frame_bytes(89, 6), 760);
        // 760 bytes = 24 clear instructions; the call saves 40 + 24.
        assert!(link_pays_for_its_frame(1000, 760));
        assert!(link_pays_for_its_frame(375, 760));
        assert!(!link_pays_for_its_frame(374, 760));
        assert!(!link_pays_for_its_frame(0, 760));
        // A small frame pays at a small share; a frameless body always.
        assert!(link_pays_for_its_frame(100, 64));
        assert!(link_pays_for_its_frame(0, 0));
        // The entries come from the parent's samples over its invocations.
        let mut parent = start_like();
        assert_eq!(site_entry_permille(&parent, 5), None);
        parent.callee_invocations = 200;
        parent.callee_receiver_profile = vec![(5, samples(1000))];
        assert_eq!(site_entry_permille(&parent, 5), Some(5000));
        assert_eq!(site_entry_permille(&parent, 9), Some(0));
    }



