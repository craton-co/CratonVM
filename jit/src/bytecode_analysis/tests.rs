use super::*;
use cratonvm_reader::instruction::Instruction;

/// The class reader's decoder is the oracle: the verifier and the IR
/// builder already trust it, so the JIT's table must agree byte for byte.
fn reader_len(code: &[u8], pc: usize) -> Option<usize> {
    Instruction::decode(code, pc)
        .ok()
        .map(|(_, next)| next - pc)
}

/// Operand bytes that make each fixed-width opcode decodable.
fn operands_for(op: u8) -> Vec<u8> {
    match op {
        0xba => vec![op, 0x00, 0x01, 0x00, 0x00], // invokedynamic: two zero bytes
        0xb9 => vec![op, 0x00, 0x01, 0x01, 0x00], // invokeinterface: count 1, zero
        0xbc => vec![op, 0x0a],                   // newarray T_INT
        0xc5 => vec![op, 0x00, 0x01, 0x01],       // multianewarray, 1 dimension
        _ => vec![op, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00],
    }
}

#[test]
fn every_fixed_width_opcode_matches_the_class_reader() {
    let mut compared = 0;
    for op in 0x00u8..=0xc9 {
        if matches!(op, 0xaa | 0xab | 0xc4) {
            continue;
        }
        let code = operands_for(op);
        let Some(expected) = reader_len(&code, 0) else {
            continue;
        };
        assert_eq!(insn_len(&code, 0), Some(expected), "opcode {op:#04x}");
        compared += 1;
    }
    // 0x00..=0xc9 is 202 opcodes; three are variable-width.
    assert_eq!(compared, 199, "the reader must decode every other opcode");
}

#[test]
fn jvms_widths_that_drifted_before_are_pinned() {
    // Each of these was missing from some private table at some point.
    let cases: &[(&[u8], usize)] = &[
        (&[0x12, 0xbb], 2),                   // ldc (CM-FASTMATH)
        (&[0x13, 0x00, 0xbb], 3),             // ldc_w
        (&[0x14, 0x00, 0xbb], 3),             // ldc2_w
        (&[0xa8, 0x00, 0x10], 3),             // jsr (loop_analysis lacked it)
        (&[0xa9, 0x01], 2),                   // ret (loop_analysis lacked it)
        (&[0xba, 0x00, 0x01, 0x00, 0x00], 5), // invokedynamic
        (&[0xc8, 0x00, 0x00, 0x00, 0x10], 5), // goto_w
        (&[0xc9, 0x00, 0x00, 0x00, 0x10], 5), // jsr_w
        (&[0xc5, 0x00, 0x01, 0x02], 4),       // multianewarray
    ];
    for &(code, len) in cases {
        assert_eq!(insn_len(code, 0), Some(len), "{code:02x?}");
    }
}

#[test]
fn wide_forms_match_the_class_reader() {
    for op in [
        0x15u8, 0x16, 0x17, 0x18, 0x19, 0x36, 0x37, 0x38, 0x39, 0x3a, 0xa9,
    ] {
        let code = [0xc4, op, 0x01, 0x00];
        assert_eq!(insn_len(&code, 0), Some(4), "wide {op:#04x}");
        assert_eq!(reader_len(&code, 0), Some(4), "reader wide {op:#04x}");
        let insn = decode_at(&code, code.len(), 0).expect("decodes");
        assert!(insn.wide);
        assert_eq!((insn.opcode, insn.op), (0xc4, op));
    }
    let iinc = [0xc4, 0x84, 0x01, 0x00, 0x00, 0x05];
    assert_eq!(insn_len(&iinc, 0), Some(6));
    assert_eq!(reader_len(&iinc, 0), Some(6));
    // A truncated prefix reads as the 4-byte form, but does not decode.
    assert_eq!(insn_len(&[0xc4], 0), Some(4));
    assert_eq!(decode_at(&[0xc4], 1, 0), None);
    // `wide` may not modify an arbitrary opcode.
    assert_eq!(decode_at(&[0xc4, 0x60, 0x00, 0x00], 4, 0), None);
}

/// `nop * lead` then a switch whose targets are all `target_back` bytes
/// before the switch opcode (0 = the switch itself).
fn tableswitch_at(lead: usize, low: i32, high: i32) -> (Vec<u8>, usize) {
    let mut code = vec![0x00; lead];
    let pc = code.len();
    code.push(0xaa);
    while code.len() % 4 != 0 {
        code.push(0x00);
    }
    let off = -(pc as i32); // every target is pc 0
    code.extend_from_slice(&off.to_be_bytes());
    code.extend_from_slice(&low.to_be_bytes());
    code.extend_from_slice(&high.to_be_bytes());
    for _ in low..=high {
        code.extend_from_slice(&off.to_be_bytes());
    }
    (code, pc)
}

fn lookupswitch_at(lead: usize, keys: &[i32]) -> (Vec<u8>, usize) {
    let mut code = vec![0x00; lead];
    let pc = code.len();
    code.push(0xab);
    while code.len() % 4 != 0 {
        code.push(0x00);
    }
    let off = -(pc as i32);
    code.extend_from_slice(&off.to_be_bytes());
    code.extend_from_slice(&(keys.len() as i32).to_be_bytes());
    for &k in keys {
        code.extend_from_slice(&k.to_be_bytes());
        code.extend_from_slice(&off.to_be_bytes());
    }
    (code, pc)
}

#[test]
fn switch_padding_is_right_at_every_alignment() {
    for lead in 0..8 {
        let (code, pc) = tableswitch_at(lead, 3, 5);
        let pad = (4 - (pc + 1) % 4) % 4;
        let expected = 1 + pad + 12 + 3 * 4;
        assert_eq!(code.len() - pc, expected, "fixture, lead {lead}");
        assert_eq!(
            insn_len(&code, pc),
            Some(expected),
            "tableswitch lead {lead}"
        );
        assert_eq!(
            reader_len(&code, pc),
            Some(expected),
            "reader tableswitch lead {lead}"
        );
        let table = switch_table(&code, code.len(), pc).expect("tableswitch decodes");
        assert_eq!(table.len, expected);
        assert_eq!(table.default, 0);
        assert_eq!(table.cases, vec![(3, 0), (4, 0), (5, 0)]);

        let (code, pc) = lookupswitch_at(lead, &[-7, 9]);
        let pad = (4 - (pc + 1) % 4) % 4;
        let expected = 1 + pad + 8 + 2 * 8;
        assert_eq!(
            insn_len(&code, pc),
            Some(expected),
            "lookupswitch lead {lead}"
        );
        assert_eq!(
            reader_len(&code, pc),
            Some(expected),
            "reader lookupswitch lead {lead}"
        );
        let table = switch_table(&code, code.len(), pc).expect("lookupswitch decodes");
        assert_eq!(table.cases, vec![(-7, 0), (9, 0)]);
        assert_eq!(table.targets().collect::<Vec<_>>(), vec![0, 0, 0]);
    }
}

#[test]
fn malformed_switches_refuse_strictly_and_step_one_byte() {
    // No room for the header.
    let bare = [0xaa, 0x00, 0x00];
    assert_eq!(insn_len(&bare, 0), None);
    assert_eq!(step(&bare, 0), 1);
    assert_eq!(switch_table(&bare, bare.len(), 0), None);
    assert!(switch_targets_lenient(&bare, bare.len(), 0).is_empty());

    // high < low.
    let (mut code, pc) = tableswitch_at(0, 5, 5);
    code[pc + 8..pc + 12].copy_from_slice(&6i32.to_be_bytes()); // low = 6 > high
    assert_eq!(insn_len(&code, pc), None);
    assert_eq!(switch_table(&code, code.len(), pc), None);
    assert_eq!(
        switch_targets_lenient(&code, code.len(), pc),
        vec![0],
        "default only"
    );

    // Negative npairs.
    let mut neg = vec![0xab, 0x00, 0x00, 0x00];
    neg.extend_from_slice(&0i32.to_be_bytes());
    neg.extend_from_slice(&(-5i32).to_be_bytes());
    assert_eq!(insn_len(&neg, 0), None);
    assert_eq!(step(&neg, 0), 1);

    // One case target out of range: strict refuses, lenient keeps the rest.
    let (mut code, pc) = tableswitch_at(1, 0, 1);
    let last = code.len() - 4;
    code[last..].copy_from_slice(&1000i32.to_be_bytes());
    assert_eq!(switch_table(&code, code.len(), pc), None);
    assert_eq!(switch_targets_lenient(&code, code.len(), pc), vec![0, 0]);
    let mut out = Vec::new();
    assert!(!explicit_targets(&code, code.len(), pc, &mut out));
    assert!(out.is_empty(), "nothing appended on refusal");

    // Table runs past `code_len`: strict refuses, lenient stops.
    let (code, pc) = tableswitch_at(0, 0, 2);
    let short = code.len() - 4;
    assert_eq!(switch_table(&code, short, pc), None);
    assert_eq!(switch_targets_lenient(&code, short, pc), vec![0, 0, 0]);
}

#[test]
fn offset_branches_decode_both_widths() {
    // nop; nop; goto -2
    let code = [0x00, 0x00, 0xa7, 0xff, 0xfe];
    assert_eq!(offset_branch_target(&code, 2), Some(0));
    // goto_w +5 from pc 0 lands past the end: decoded but not in range.
    let w = [0xc8, 0x00, 0x00, 0x00, 0x05];
    assert_eq!(offset_branch_target(&w, 0), Some(5));
    let mut out = Vec::new();
    assert!(!explicit_targets(&w, w.len(), 0, &mut out));
    // A negative target is no target.
    assert_eq!(offset_branch_target(&[0xa7, 0xff, 0xf0], 0), None);
    // Not a branch.
    assert_eq!(offset_branch_target(&[0x00], 0), None);
    // Truncated offset.
    assert_eq!(offset_branch_target(&[0x99, 0x00], 0), None);
}

#[test]
fn subroutines_are_opaque_and_exits_do_not_fall_through() {
    let mut out = Vec::new();
    for code in [
        &[0xa8u8, 0x00, 0x03][..],
        &[0xa9, 0x01][..],
        &[0xc9, 0, 0, 0, 5][..],
    ] {
        assert!(
            !explicit_targets(code, code.len(), 0, &mut out),
            "{code:02x?}"
        );
        assert!(is_subroutine_op(code[0]));
    }
    for op in [
        0xa7u8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xbf, 0xc8,
    ] {
        assert!(!falls_through(op), "{op:#04x}");
    }
    for op in [0x00u8, 0x99, 0xa6, 0xa8, 0xb6, 0xc6, 0xc7, 0xc9] {
        assert!(falls_through(op), "{op:#04x}");
    }
    assert!(is_exit(0xbf) && is_exit(0xb1) && !is_exit(0xa7));
}

/// `iconst_0; istore_1; L2: iload_1; bipush 10; if_icmpge L15;
/// iinc 1,1; goto L2; L15: return`
fn top_tested_loop() -> Vec<u8> {
    vec![
        0x03, 0x3c, // 0: iconst_0, 1: istore_1
        0x1b, // 2: iload_1
        0x10, 0x0a, // 3: bipush 10
        0xa2, 0x00, 0x0a, // 5: if_icmpge +10 -> 15
        0x84, 0x01, 0x01, // 8: iinc 1 1
        0xa7, 0xff, 0xf7, // 11: goto -9 -> 2
        0x00, // 14: nop (dead)
        0xb1, // 15: return
    ]
}

#[test]
fn walks_maps_and_decoded_method_agree() {
    let code = top_tested_loop();
    let insns = decode_method(&code, code.len()).expect("decodes");
    let pcs: Vec<usize> = insns.iter().map(|i| i.pc).collect();
    assert_eq!(pcs, vec![0, 1, 2, 3, 5, 8, 11, 14, 15]);
    let starts = instruction_starts(&code, code.len());
    for (pc, &s) in starts.iter().enumerate() {
        assert_eq!(s, pcs.contains(&pc), "pc {pc}");
    }
    // The reader's canonical walk sees the same boundaries.
    let verified = cratonvm_reader::verified_code(&code).expect("verifies");
    for vi in verified.instructions() {
        let pc = vi.pc as usize;
        assert_eq!(
            insn_len(&code, pc),
            Some(vi.next_pc as usize - pc),
            "pc {pc}"
        );
    }
    let targets = branch_target_map(&code, code.len());
    let marked: Vec<usize> = (0..code.len()).filter(|&p| targets[p]).collect();
    assert_eq!(marked, vec![2, 15]);
    assert_eq!(back_edges(&code, code.len()), vec![(2, 11)]);
    // Overrunning walk: truncate inside `iinc`.
    assert_eq!(decode_method(&code, 10), None);
}

#[test]
fn reachability_skips_dead_code_and_honours_extra_roots() {
    let code = top_tested_loop();
    let r = reachable_pcs(&code, code.len(), &[]).expect("known control flow");
    assert!(r[0] && r[2] && r[8] && r[11] && r[15]);
    assert!(!r[14], "the nop after goto is dead");
    let r = reachable_pcs(&code, code.len(), &[14]).expect("known control flow");
    assert!(r[14]);
    // jsr makes it opaque.
    assert_eq!(reachable_pcs(&[0xa8, 0x00, 0x03, 0xb1], 4, &[]), None);
}

#[test]
fn cfg_finds_the_header_and_body_of_a_top_tested_loop() {
    let code = top_tested_loop();
    let cfg = InsnCfg::build(&code, code.len()).expect("cfg builds");
    let n = |pc| cfg.node_of(pc).unwrap();
    assert_eq!(cfg.loop_headers(), vec![n(2)]);
    assert!(cfg.dominates(n(2), n(11)));
    assert!(cfg.dominates(n(5), n(15)));
    assert!(!cfg.is_reachable(n(14)));
    assert!(
        !cfg.dominates(n(14), n(15)),
        "unreachable dominates nothing"
    );
    assert_eq!(cfg.idom(n(8)), Some(n(5)));
    assert_eq!(cfg.idom(n(0)), None);
    let body = cfg.natural_loop(n(2), n(11)).expect("natural loop");
    let body_pcs: Vec<usize> = cfg
        .nodes()
        .iter()
        .enumerate()
        .filter(|&(i, _)| body[i])
        .map(|(_, &pc)| pc)
        .collect();
    assert_eq!(body_pcs, vec![2, 3, 5, 8, 11]);
    assert_eq!(cfg.rpo()[0], 0);
    assert_eq!(cfg.preds(n(2)).len(), 2, "entry fall-through and the goto");
    assert_eq!(
        cfg.natural_loop(n(5), n(11)),
        None,
        "5 -> 11 is not a back edge"
    );
}

#[test]
fn a_rotated_loop_is_headed_by_its_test_not_by_the_backward_target() {
    // 0: goto L8; L3: iinc 1,1; L6: nop; L7: nop;
    // L8: iload_1; L9: bipush 10; L11: if_icmplt L3; L14: return
    let code = vec![
        0xa7, 0x00, 0x08, // 0: goto +8 -> 8
        0x84, 0x01, 0x01, // 3: iinc 1 1
        0x00, // 6: nop
        0x00, // 7: nop
        0x1b, // 8: iload_1
        0x10, 0x0a, // 9: bipush 10
        0xa1, 0xff, 0xf8, // 11: if_icmplt -8 -> 3
        0xb1, // 14: return
    ];
    // Textually the backward branch targets the body start...
    assert_eq!(back_edges(&code, code.len()), vec![(3, 11)]);
    // ...but the body start does not dominate the test (the entry `goto`
    // bypasses it); the test dominates the body.
    let cfg = InsnCfg::build(&code, code.len()).expect("cfg builds");
    let n = |pc| cfg.node_of(pc).unwrap();
    assert!(!cfg.dominates(n(3), n(11)));
    assert!(cfg.dominates(n(8), n(3)));
    assert_eq!(cfg.loop_headers(), vec![n(8)]);
    let body = cfg
        .natural_loop(n(8), n(7))
        .expect("7 -> 8 is the back edge");
    for pc in [3, 6, 7, 8, 9, 11] {
        assert!(body[n(pc)], "pc {pc} in the body");
    }
    assert!(!body[n(0)] && !body[n(14)]);
}

#[test]
fn nested_loops_have_nested_headers() {
    // for (i = 0; i < 4; i++) { for (j = 0; j < 4; j++) {} }
    let code = vec![
        0x03, 0x3c, // 0: iconst_0, 1: istore_1
        0x1b, 0x10, 0x04, // 2: iload_1, 3: bipush 4
        0xa2, 0x00, 0x17, // 5: if_icmpge +23 -> 28
        0x03, 0x3d, // 8: iconst_0, 9: istore_2
        0x1c, 0x10, 0x04, // 10: iload_2, 11: bipush 4
        0xa2, 0x00, 0x09, // 13: if_icmpge +9 -> 22
        0x84, 0x02, 0x01, // 16: iinc 2 1
        0xa7, 0xff, 0xf7, // 19: goto -9 -> 10
        0x84, 0x01, 0x01, // 22: iinc 1 1
        0xa7, 0xff, 0xe9, // 25: goto -23 -> 2
        0xb1, // 28: return
    ];
    let cfg = InsnCfg::build(&code, code.len()).expect("cfg builds");
    let n = |pc| cfg.node_of(pc).unwrap();
    assert_eq!(cfg.loop_headers(), vec![n(2), n(10)]);
    assert!(cfg.dominates(n(2), n(10)));
    assert!(!cfg.dominates(n(10), n(2)));
    let outer = cfg.natural_loop(n(2), n(25)).expect("outer loop");
    let inner = cfg.natural_loop(n(10), n(19)).expect("inner loop");
    assert!(outer[n(10)] && outer[n(19)] && outer[n(22)]);
    assert!(inner[n(16)] && !inner[n(22)] && !inner[n(2)]);
    assert_eq!(back_edges(&code, code.len()), vec![(10, 19), (2, 25)]);
}

#[test]
fn handler_edges_make_the_handler_reachable_and_dominated_by_the_try() {
    // 0: iconst_1; 1: istore_1; 2: iload_1; 3: ireturn;
    // 4: astore_2 (handler for [0, 4)); 5: iconst_m1; 6: ireturn
    let code = vec![0x04, 0x3c, 0x1b, 0xac, 0x4d, 0x02, 0xac];
    let plain = InsnCfg::build(&code, code.len()).expect("cfg builds");
    assert!(!plain.is_reachable(plain.node_of(4).unwrap()));
    let cfg =
        InsnCfg::build_with_handlers(&code, code.len(), &[(0, 4, 4)]).expect("cfg builds");
    let n = |pc| cfg.node_of(pc).unwrap();
    assert!(cfg.is_reachable(n(4)));
    assert_eq!(cfg.preds(n(4)).len(), 4, "every protected instruction");
    assert!(cfg.dominates(n(0), n(4)));
    assert_eq!(cfg.idom(n(4)), Some(n(0)));
    assert!(cfg.loop_headers().is_empty());
    // A handler pc inside an instruction refuses.
    let mid = [0x10, 0x05, 0xac];
    assert!(InsnCfg::build_with_handlers(&mid, 3, &[(0, 2, 1)]).is_none());
}

#[test]
fn switch_back_edges_are_counted_once_per_switch() {
    // nop; tableswitch low 0 high 2, every target pc 0.
    let (code, pc) = tableswitch_at(1, 0, 2);
    assert_eq!(back_edges(&code, code.len()), vec![(0, pc)]);
    let map = branch_target_map(&code, code.len());
    assert!(map[0]);
    assert_eq!(map.iter().filter(|&&b| b).count(), 1);
}

/// `wide ret` is a `ret`. Its opcode byte is `0xc4`, which no opcode-keyed
/// predicate here treats as a subroutine op or a terminator, so the strict
/// successor functions used to call it an ordinary instruction that falls
/// through — exactly the opaque control flow they exist to refuse.
#[test]
fn wide_ret_is_as_opaque_as_ret() {
    // 0: wide ret 1   4: return
    let code = [0xc4, 0xa9, 0x00, 0x01, 0xb1];
    assert!(is_wide_ret(&code, 0));
    assert!(!is_wide_ret(&code, 4));
    assert!(
        !is_wide_ret(&[0xc4, 0x84, 0, 1, 0, 1], 0),
        "wide iinc is not"
    );
    let mut out = Vec::new();
    assert!(!explicit_targets(&code, code.len(), 0, &mut out));
    assert!(out.is_empty(), "nothing appended on refusal");
    assert!(!normal_successors(&code, code.len(), 0, &mut out));
    assert_eq!(reachable_pcs(&code, code.len(), &[]), None);
    assert!(InsnCfg::build(&code, code.len()).is_none());
    // The lenient view matches narrow `ret`: no successors at all.
    assert!(lenient_successors(&code, code.len(), 0).is_empty());
    assert!(lenient_successors(&[0xa9, 0x01, 0xb1], 3, 0).is_empty());
    // Every other widened form still falls through.
    let wide_iload = [0xc4, 0x15, 0x00, 0x01, 0xac];
    assert!(explicit_targets(&wide_iload, 5, 0, &mut out));
    assert_eq!(lenient_successors(&wide_iload, 5, 0), vec![4]);
}
