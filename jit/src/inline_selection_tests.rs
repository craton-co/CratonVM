use super::*;

fn site(code_len: usize, fields: usize, static_fields: usize, needs_heap: bool) -> InlineSite {
    InlineSite {
        callee_original_code: None,
        callee_original_handler_rows: Vec::new(),
        callee_always_taken_pcs: Vec::new(),
        callee_pruned_switch_edges: Vec::new(),
        callee_code: vec![0; code_len.saturating_add(2)],
        callee_code_len: code_len,
        callee_max_locals: 1,
        callee_num_args: 0,
        callee_is_static: true,
        return_type: b'V',
        field_info: (0..fields).map(|pc| (pc, 0, b'I')).collect(),
        field_decl_info: Vec::new(),
        compact_field_info: Vec::new(),
        static_field_info: (0..static_fields)
            .map(|pc| (pc, 1, 0, b'I', false))
            .collect(),
        ldc_info: Vec::new(),
        ldc2w_info: Vec::new(),
        ldc_fp_pcs: Vec::new(),
        needs_heap,
        class_name: "InlineCost".to_string(),
        class_id: 0,
        method_name: "leaf".to_string(),
        descriptor: "()V".to_string(),
        elided_invoke_pcs: Vec::new(),
        invoke_targets: Vec::new(),
        unloaded_static_call_pcs: Vec::new(),
        resolved_invoke_infos: Vec::new(),
        nested_sites: Vec::new().into(),
        ir_new_info: Vec::new(),
        ir_anewarray_info: Vec::new(),
        ir_typecheck_info: Vec::new(),
        ir_typecheck_final: Vec::new(),
        volatile_field_pcs: Vec::new(),
        final_field_pcs: Vec::new(),
        byte_array_field_pcs: Vec::new(),
        referent_load_pcs: Vec::new(),
        ir_ldc_ref_rows: Vec::new(),
        ir_getclass_compares: Vec::new(),
        callee_is_synchronized: false,
        ir_ldc_class_info: Vec::new(),
        ir_handler_rows: Vec::new(),
        ir_indy_rows: Vec::new(),
        ir_indy_capture_rows: Vec::new(),
        ir_indy_capture_sites: Vec::new(),
        ir_lambda_impl_rows: Vec::new(),
        callee_receiver_profile: Vec::new(),
        callee_never_taken_pcs: Vec::new(),
        callee_cold_arm_ranges: Vec::new(),
        callee_hot_path_pcs: Vec::new(),
        callee_invocations: 0,
        hidden: None,
        cha_class_set: Vec::new(),
        ir_caller_mirror_slot: 0,
        ir_security_walk_ignored: false,
    }
}

#[test]
fn inline_selection_prices_backend_expansion() {
    assert_eq!(inline_site_expansion_cost(&site(10, 2, 1, true)), Some(34));
}

#[test]
fn inline_selection_rejects_oversized_or_expensive_leaf() {
    assert_eq!(
        inline_site_expansion_cost(&site(MAX_INLINE_BYTECODE_SIZE + 1, 0, 0, false)),
        None
    );
    assert_eq!(inline_site_expansion_cost(&site(35, 5, 0, true)), None);
}

// ── HotSpot three-tier shape (jit-inlining-and-ir-calls) ────────────

/// A callee between `MaxInlineSize` (35) and `FreqInlineSize` (325) is
/// rejected at a cold site and accepted at a hot one. This is the whole
/// point of the re-shape: before it, the flat 35-byte cap meant a 200-byte
/// method in the middle of a hot loop was treated exactly like one called
/// twice.
#[test]
fn hot_site_gets_freq_inline_size_allowance() {
    let s = site(200, 0, 0, false);
    assert_eq!(
        inline_site_expansion_cost_tiered(&s, false),
        None,
        "200 bytes must exceed MaxInlineSize at a cold site"
    );
    assert_eq!(
        inline_site_expansion_cost_tiered(&s, true),
        Some(200),
        "200 bytes is within FreqInlineSize at a hot site"
    );
    // The hot tier is still bounded — FreqInlineSize is a cap, not a licence.
    assert_eq!(
        inline_site_expansion_cost_tiered(
            &site(MAX_INLINE_BYTECODE_SIZE + 1, 0, 0, false),
            true
        ),
        None
    );
}

/// The cold tier is byte-for-byte the pre-re-shape behaviour, so an
/// unprofiled compile inlines identically. `inline_site_expansion_cost`
/// must agree with the tiered form at `site_is_hot == false`.
#[test]
fn cold_tier_matches_legacy_behaviour() {
    for (len, f, sf, heap) in [
        (10usize, 2usize, 1usize, true),
        (35, 0, 0, false),
        (36, 0, 0, false),
        (35, 5, 0, true),
    ] {
        let s = site(len, f, sf, heap);
        assert_eq!(
            inline_site_expansion_cost(&s),
            inline_site_expansion_cost_tiered(&s, false),
            "legacy entry point must delegate to the cold tier"
        );
    }
    // 36 bytes was rejected by the old flat cap and must still be rejected
    // when cold, even though the admission constant is now 325.
    assert_eq!(inline_site_expansion_cost(&site(36, 0, 0, false)), None);
}

/// A trivial callee (<= `MaxTrivialSize`) bypasses the SIZE cap in either
/// tier — but not the expansion ceiling, so a tiny body doing many field
/// loads is still priced honestly rather than waved through.
#[test]
fn trivial_callee_bypasses_size_cap_but_not_expansion_cap() {
    assert_eq!(
        inline_site_expansion_cost_tiered(&site(MAX_TRIVIAL_INLINE_SIZE, 0, 0, false), false),
        Some(MAX_TRIVIAL_INLINE_SIZE)
    );
    // 6-byte body, 10 field accesses → 6 + 60 = 66 > 64: rejected cold.
    assert_eq!(
        inline_site_expansion_cost_tiered(&site(MAX_TRIVIAL_INLINE_SIZE, 10, 0, false), false),
        None
    );
    // …but within the hot ceiling of 512.
    assert_eq!(
        inline_site_expansion_cost_tiered(&site(MAX_TRIVIAL_INLINE_SIZE, 10, 0, false), true),
        Some(66)
    );
}

/// `hot_loop_ranges` must recover each loop's HEADER by decoding the
/// back-edge branch, because `LoopTripProfile` records only the back-edge
/// pc. A site inside the recovered `[header, back_edge]` span is hot; one
/// outside it is not.
#[test]
fn hot_loop_range_recovers_header_from_backedge_branch() {
    // pc 0..7: loop body (a call site at pc 4)
    // pc 8:    goto -8  → back to pc 0   (0xa7, 0xff, 0xf8)
    // pc 11:   a call site AFTER the loop
    let mut code = vec![0u8; 16];
    code[8] = 0xa7;
    code[9] = 0xff;
    code[10] = 0xf8; // -8
    let mut prof = profile::MethodProfile::default();
    for _ in 0..INLINE_HOT_LOOP_BACKEDGES {
        prof.record_backedge(8);
    }
    let ranges = hot_loop_ranges(&code, code.len(), Some(&prof));
    assert_eq!(
        ranges,
        vec![(0usize, 8usize)],
        "header recovered from goto -8"
    );
    assert!(
        call_site_is_hot(4, &ranges, Some(&prof)),
        "site inside the loop is hot"
    );
    assert!(
        !call_site_is_hot(11, &ranges, Some(&prof)),
        "site after the loop is cold"
    );
}

/// A cold loop (below the back-edge threshold) yields no hot range, and a
/// FORWARD branch keyed in the profile is ignored rather than mistaken for
/// a back-edge — a forward target would otherwise produce an inverted span.
#[test]
fn hot_loop_ranges_ignore_cold_loops_and_forward_branches() {
    let mut code = vec![0u8; 16];
    code[8] = 0xa7;
    code[9] = 0xff;
    code[10] = 0xf8; // -8, a real back-edge
    let mut cold = profile::MethodProfile::default();
    for _ in 0..(INLINE_HOT_LOOP_BACKEDGES - 1) {
        cold.record_backedge(8);
    }
    assert!(hot_loop_ranges(&code, code.len(), Some(&cold)).is_empty());

    let mut forward_code = vec![0u8; 16];
    forward_code[2] = 0xa7;
    forward_code[3] = 0x00;
    forward_code[4] = 0x06; // +6 → forward
    let mut hot = profile::MethodProfile::default();
    for _ in 0..INLINE_HOT_LOOP_BACKEDGES {
        hot.record_backedge(2);
    }
    assert!(hot_loop_ranges(&forward_code, forward_code.len(), Some(&hot)).is_empty());

    // No profile at all ⇒ nothing hot ⇒ every budget is the legacy one.
    assert!(hot_loop_ranges(&code, code.len(), None).is_empty());
    assert!(!call_site_is_hot(4, &[], None));
}

/// Round 14 wave 3 (lane calls): `ifnull` / `ifnonnull` back edges close a
/// hot loop (`do { .. } while (p != null)`), and a backward `jsr` does not.
#[test]
fn hot_loop_ranges_take_null_test_back_edges_and_refuse_jsr() {
    let hot_at = |pc: usize| {
        let mut prof = profile::MethodProfile::default();
        for _ in 0..INLINE_HOT_LOOP_BACKEDGES {
            prof.record_backedge(pc);
        }
        prof
    };
    for op in [0xc6u8, 0xc7] {
        let mut code = vec![0u8; 16];
        code[8] = op;
        code[9] = 0xff;
        code[10] = 0xf8; // -8
        assert_eq!(
            hot_loop_ranges(&code, code.len(), Some(&hot_at(8))),
            vec![(0, 8)],
            "opcode {op:#x}"
        );
    }
    let mut jsr = vec![0u8; 16];
    jsr[8] = 0xa8;
    jsr[9] = 0xff;
    jsr[10] = 0xf8;
    assert!(hot_loop_ranges(&jsr, jsr.len(), Some(&hot_at(8))).is_empty());
}

/// Round 14 wave 5 (lane fixup2, RV4-4): the four loop-range readers share
/// `loop_back_edge_header`, so a `goto_w` loop is the same loop to all of
/// them, a backward `jsr_w` is a loop to none, operands cut off by
/// `code_len` decode to nothing, and a `code_len` past the slice does not
/// index out of bounds.
#[test]
fn loop_range_readers_agree_on_goto_w_and_refuse_jsr_w() {
    let hot_at = |pc: usize| {
        let mut prof = profile::MethodProfile::default();
        for _ in 0..INLINE_HOT_LOOP_BACKEDGES {
            prof.record_backedge(pc);
        }
        prof
    };
    // pc 0..7: loop body; pc 8: goto_w -8 (5 bytes); pc 13: after the loop.
    let mut code = vec![0u8; 16];
    code[8..13].copy_from_slice(&[0xc8, 0xff, 0xff, 0xff, 0xf8]);
    assert_eq!(loop_back_edge_header(&code, code.len(), 8), Some(0));
    assert_eq!(
        hot_loop_ranges(&code, code.len(), Some(&hot_at(8))),
        vec![(0, 8)]
    );
    assert_eq!(bytecode_loop_ranges(&code, code.len()), vec![(0, 8)]);
    assert_eq!(osr_entry_loop_ranges(&code, code.len(), 8), vec![(0, 8)]);
    assert_eq!(osr_entry_loop_ranges(&code, code.len(), 4), vec![(0, 8)]);
    assert!(bytecode_pc_in_loop(&code, code.len(), 4));
    assert!(!bytecode_pc_in_loop(&code, code.len(), 13));
    // Operands past `code_len`: not a branch.
    assert_eq!(loop_back_edge_header(&code, 12, 8), None);
    assert!(hot_loop_ranges(&code, 12, Some(&hot_at(8))).is_empty());
    // A `code_len` beyond the slice is clamped, never indexed.
    assert!(hot_loop_ranges(&code, 64, Some(&hot_at(40))).is_empty());
    assert_eq!(hot_loop_ranges(&code, 64, Some(&hot_at(8))), vec![(0, 8)]);

    let mut jsr_w = vec![0u8; 16];
    jsr_w[8..13].copy_from_slice(&[0xc9, 0xff, 0xff, 0xff, 0xf8]);
    assert_eq!(loop_back_edge_header(&jsr_w, jsr_w.len(), 8), None);
    assert!(hot_loop_ranges(&jsr_w, jsr_w.len(), Some(&hot_at(8))).is_empty());
    assert!(bytecode_loop_ranges(&jsr_w, jsr_w.len()).is_empty());
    assert!(!bytecode_pc_in_loop(&jsr_w, jsr_w.len(), 4));
}

/// The second hotness signal: a call site with a heavily-executed
/// receiver-type profile is hot even with no enclosing loop.
#[test]
fn hot_receiver_profile_marks_a_site_hot() {
    let mut prof = profile::MethodProfile::default();
    for _ in 0..INLINE_HOT_SITE_OBSERVATIONS {
        prof.record_receiver(4, 7);
    }
    assert!(call_site_is_hot(4, &[], Some(&prof)));
    assert!(!call_site_is_hot(5, &[], Some(&prof)));

    let mut lukewarm = profile::MethodProfile::default();
    for _ in 0..(INLINE_HOT_SITE_OBSERVATIONS - 1) {
        lukewarm.record_receiver(4, 7);
    }
    assert!(!call_site_is_hot(4, &[], Some(&lukewarm)));
}


/// A static call the resolver rowed only for an arm its prune feed called dead
/// (`if (x == 0) return; Q.m();` with the fall-through never taken) is a plain
/// call for a builder that declines the prune: it does not make the body
/// unbindable, and the body is still a leaf. Rowed live, the same call does.
#[test]
fn a_prune_dead_call_row_is_neither_unbindable_nor_a_call() {
    // 0 iload_0; 1 ifeq +6 (-> 7); 4 invokestatic #1; 7 return; 2 sentinels.
    let code = vec![0x1a, 0x99, 0x00, 0x06, 0xb8, 0x00, 0x01, 0xb1, 0, 0];
    let mut site = InlineSite {
        callee_code: code,
        callee_code_len: 8,
        callee_is_static: true,
        invoke_targets: vec![(
            4,
            InlineInvokeTarget {
                class_name: "p/Q".into(),
                method_name: "m".into(),
                descriptor: "()V".into(),
                invoke_kind: 3,
                ..Default::default()
            },
        )],
        ..Default::default()
    };
    assert!(ir_splice_body_has_unbindable_call(&site), "live row is unbindable");
    assert!(!ir_nested_hot_body_is_leaf(&site), "live row is a call");
    site.callee_always_taken_pcs = vec![1];
    assert!(!ir_splice_body_has_unbindable_call(&site));
    assert!(ir_nested_hot_body_is_leaf(&site));
}

/// javac's varargs boxing of a reflective call keeps its `valueOf` a call;
/// the same box handed to a proxy's handler, or not stored into an array,
/// keeps its splice.
#[test]
fn a_reflective_varargs_box_stays_a_call_and_nothing_else() {
    // 0: iload_0  1: invokestatic valueOf  4: aastore  5: invokevirtual ...  8
    let code = [0x1a, 0xb8, 0, 1, 0x53, 0xb6, 0, 2, 0xb1];
    let value_of = ("java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;");
    let invoke = Some(("java/lang/reflect/Method", "invoke"));
    assert!(reflective_varargs_box_site(&code, 9, 1, value_of, invoke));
    assert!(!reflective_varargs_box_site(
        &code,
        9,
        1,
        value_of,
        Some(("java/lang/reflect/InvocationHandler", "invoke"))
    ));
    let not_stored = [0x1a, 0xb8, 0, 1, 0x57, 0xb6, 0, 2, 0xb1];
    assert!(!reflective_varargs_box_site(&not_stored, 9, 1, value_of, invoke));
    assert!(!reflective_varargs_box_site(
        &code,
        9,
        1,
        ("java/lang/Integer", "parseInt", "(Ljava/lang/String;)I"),
        invoke
    ));
}
