use super::*;

#[test]
fn modified_locals_finds_iinc_and_stores() {
    // istore_1 ; iinc 2, 1 ; istore_3 ; nop
    let code: Vec<u8> = vec![0x3c, 0x84, 0x02, 0x01, 0x3e, 0x00];
    let m = modified_locals_strict(&code, 0, code.len()).unwrap_or(u64::MAX);
    // bits 1, 2, 3 set
    assert_eq!(m, 0b1110);
}

/// A `wide istore` decodes as the opcode `0xc4`, which the old scanner's
/// `_` arm marked nothing for — and `step` then walked past the whole
/// four-byte instruction, so the write was simply absent from the set and
/// the local read as loop-invariant.
#[test]
fn a_wide_store_is_not_invisible_to_the_modified_set() {
    // wide istore 5 ; nop
    let code: Vec<u8> = vec![0xc4, 0x36, 0x00, 0x05, 0x00];
    let m = modified_locals_strict(&code, 0, code.len()).unwrap_or(u64::MAX);
    assert_eq!(m & (1 << 5), 1 << 5, "wide istore 5 writes local 5");

    // wide iinc 6, 1 ; nop  — six bytes, and the same hole.
    let code: Vec<u8> = vec![0xc4, 0x84, 0x00, 0x06, 0x00, 0x01, 0x00];
    let m = modified_locals_strict(&code, 0, code.len()).unwrap_or(u64::MAX);
    assert_eq!(m & (1 << 6), 1 << 6, "wide iinc 6 writes local 6");
}

/// An `lstore_1` writes slots 1 AND 2. Marking only the named slot leaves
/// the dead high half looking untouched, which is the gap
/// `x64/escape_analysis.rs` carries a forty-line comment about having
/// caused a real unsound hoist.
#[test]
fn a_long_store_marks_the_dead_high_half() {
    // lstore_1 ; nop
    let code: Vec<u8> = vec![0x40, 0x00];
    let m = modified_locals_strict(&code, 0, code.len()).unwrap_or(u64::MAX);
    assert_eq!(m & 0b110, 0b110, "lstore_1 occupies locals 1 and 2");

    // dstore 4 ; nop — the two-byte form, same rule.
    let code: Vec<u8> = vec![0x39, 0x04, 0x00];
    let m = modified_locals_strict(&code, 0, code.len()).unwrap_or(u64::MAX);
    assert_eq!(m & 0b11_0000, 0b11_0000, "dstore 4 occupies locals 4 and 5");
}

/// A store this scan cannot represent is refused (`None`), never reported
/// as "no locals modified".
#[test]
fn a_store_above_the_mask_is_refused() {
    // wide astore 100 — above bit 63, so the set cannot hold it.
    let code: Vec<u8> = vec![0xc4, 0x3a, 0x00, 0x64, 0x00];
    assert_eq!(
        modified_locals_strict(&code, 0, code.len()),
        None,
        "a write the mask cannot represent blocks every hoist"
    );
}

#[test]
fn inst_len_tableswitch_padded_and_jumptable() {
    // Layout (PC shown). tableswitch must align its first operand
    // byte to a 4-byte boundary relative to code start.
    //
    // PC 0: nop                     ; force pc=1 for the switch so
    //                               ; padding is non-trivial.
    // PC 1: tableswitch (0xaa)
    //   pad PC 2,3 (operand byte aligns to PC 4)
    //   PC 4..8:   default = 0
    //   PC 8..12:  low     = 0
    //   PC 12..16: high    = 2   → n = high-low+1 = 3 jump offsets
    //   PC 16..20: jump[0]
    //   PC 20..24: jump[1]
    //   PC 24..28: jump[2]
    // operand begins at (1+1+3)&!3 = 4, so padding = PC 2,3 (2 bytes).
    // → instruction spans PC 1..28, i.e. length 27.
    let mut code: Vec<u8> = Vec::new();
    code.push(0x00); // PC 0: nop
    code.push(0xaa); // PC 1: tableswitch opcode
                     // operand begins at (1+1+3)&!3 = 4, so padding = PC 2,3 (2 bytes)
    code.push(0x00); // PC 2 padding
    code.push(0x00); // PC 3 padding
    code.extend_from_slice(&0i32.to_be_bytes()); // PC 4..8 default
    code.extend_from_slice(&0i32.to_be_bytes()); // PC 8..12 low
    code.extend_from_slice(&2i32.to_be_bytes()); // PC 12..16 high
    code.extend_from_slice(&0i32.to_be_bytes()); // PC 16..20 jump[0]
    code.extend_from_slice(&0i32.to_be_bytes()); // PC 20..24 jump[1]
    code.extend_from_slice(&0i32.to_be_bytes()); // PC 24..28 jump[2]
    assert_eq!(code.len(), 28);
    // Length of the tableswitch at PC 1 must carry the scanner to
    // PC 28 (end of code): 28 - 1 = 27.
    assert_eq!(bytecode_analysis::step(&code, 1), 27);
    // And a linear scan from PC 0 lands exactly on the end, never
    // mis-decoding a padding/offset byte as an opcode.
    let mut pc = 0;
    let mut steps = 0;
    while pc < code.len() {
        pc += bytecode_analysis::step(&code, pc);
        steps += 1;
        assert!(steps < 100, "scan failed to terminate (desync)");
    }
    assert_eq!(pc, code.len());
    assert_eq!(steps, 2); // nop, then tableswitch
}

#[test]
fn inst_len_lookupswitch_padded_and_pairs() {
    // PC 0: lookupswitch (0xab)
    //   operand at (0+1+3)&!3 = 4, so 3 padding bytes at PC 1,2,3
    //   PC 4..8:   default = 0
    //   PC 8..12:  npairs  = 2
    //   PC 12..20: pair[0] (match,offset)
    //   PC 20..28: pair[1] (match,offset)
    // → spans PC 0..28, length 28.
    let mut code: Vec<u8> = Vec::new();
    code.push(0xab); // PC 0: lookupswitch opcode
    code.push(0x00); // PC 1 padding
    code.push(0x00); // PC 2 padding
    code.push(0x00); // PC 3 padding
    code.extend_from_slice(&0i32.to_be_bytes()); // PC 4..8 default
    code.extend_from_slice(&2i32.to_be_bytes()); // PC 8..12 npairs
    code.extend_from_slice(&1i32.to_be_bytes()); // PC 12..16 match[0]
    code.extend_from_slice(&0i32.to_be_bytes()); // PC 16..20 offset[0]
    code.extend_from_slice(&2i32.to_be_bytes()); // PC 20..24 match[1]
    code.extend_from_slice(&0i32.to_be_bytes()); // PC 24..28 offset[1]
    assert_eq!(code.len(), 28);
    assert_eq!(bytecode_analysis::step(&code, 0), 28);
}

#[test]
fn inst_len_truncated_switch_is_safe() {
    // A bare tableswitch opcode with no room for the header must
    // not read out of bounds; it falls back to a 1-byte step.
    let code: Vec<u8> = vec![0xaa, 0x00, 0x00];
    assert_eq!(bytecode_analysis::step(&code, 0), 1);
    // Likewise a malformed lookupswitch with negative npairs.
    let mut bad: Vec<u8> = vec![0xab, 0x00, 0x00, 0x00];
    bad.extend_from_slice(&0i32.to_be_bytes()); // default
    bad.extend_from_slice(&(-5i32).to_be_bytes()); // npairs < 0
    assert_eq!(bytecode_analysis::step(&bad, 0), 1);
}

// -- counted-loop recognition ------------------------------------------

use crate::scev::{BoundsProof, IndexExpr, PreheaderGuard, RangeEnv, SymBound};

/// A resolver that recognises CP index 42 as `Math.min` and 43 as
/// `Math.max`, and nothing else.
fn resolver(cp: u16) -> Option<MinMax> {
    match cp {
        42 => Some(MinMax::Min),
        43 => Some(MinMax::Max),
        _ => None,
    }
}

fn no_math(_: u16) -> Option<MinMax> {
    None
}

/// `for (i = 0; i < a.length; i++) { }` with the array in local 0.
///
/// ```text
///  0: iconst_0        3: aload_0        8: iinc 1, 1
///  1: istore_1        4: arraylength   11: goto -9  → 2
///  2: iload_1         5: if_icmpge +9  14: return
/// ```
fn array_length_loop() -> Vec<u8> {
    vec![
        0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
        0xb1,
    ]
}

#[test]
fn decodes_every_bound_shape_not_just_array_length() {
    // constant
    assert_eq!(
        decode_bound_expr(&[0x10, 0x40], 0, 2, &no_math),
        Some((BoundSource::Const(64), 2))
    );
    assert_eq!(
        decode_bound_expr(&[0x03], 0, 1, &no_math),
        Some((BoundSource::Const(0), 1))
    );
    assert_eq!(
        decode_bound_expr(&[0x02], 0, 1, &no_math),
        Some((BoundSource::Const(-1), 1))
    );
    // local
    assert_eq!(
        decode_bound_expr(&[0x1c], 0, 1, &no_math),
        Some((BoundSource::Local(2), 1))
    );
    assert_eq!(
        decode_bound_expr(&[0x15, 0x09], 0, 2, &no_math),
        Some((BoundSource::Local(9), 2))
    );
    // array length
    assert_eq!(
        decode_bound_expr(&[0x2a, 0xbe], 0, 2, &no_math),
        Some((BoundSource::ArrayLength(0), 2))
    );
    // instance field
    assert_eq!(
        decode_bound_expr(&[0x2a, 0xb4, 0x00, 0x07], 0, 4, &no_math),
        Some((
            BoundSource::Field {
                cp_index: 7,
                receiver_local: Some(0)
            },
            4
        ))
    );
    // static field
    assert_eq!(
        decode_bound_expr(&[0xb2, 0x00, 0x09], 0, 3, &no_math),
        Some((
            BoundSource::Field {
                cp_index: 9,
                receiver_local: None
            },
            3
        ))
    );
    // Math.min(16, n)
    assert_eq!(
        decode_bound_expr(&[0x10, 0x10, 0x1c, 0xb8, 0x00, 0x2a], 0, 6, &resolver),
        Some((
            BoundSource::Min(
                Box::new(BoundSource::Const(16)),
                Box::new(BoundSource::Local(2))
            ),
            6
        ))
    );
    // Math.max(0, n)
    assert_eq!(
        decode_bound_expr(&[0x03, 0x1c, 0xb8, 0x00, 0x2b], 0, 5, &resolver),
        Some((
            BoundSource::Max(
                Box::new(BoundSource::Const(0)),
                Box::new(BoundSource::Local(2))
            ),
            5
        ))
    );
    // An unresolved invokestatic falls back to the first atom rather than
    // guessing what the call computed.
    assert_eq!(
        decode_bound_expr(&[0x10, 0x10, 0x1c, 0xb8, 0x00, 0x63], 0, 6, &no_math),
        Some((BoundSource::Const(16), 2))
    );
    // Nothing recognisable at all.
    assert_eq!(decode_bound_expr(&[0x60], 0, 1, &no_math), None);
}

/// Round 13 wave 3 (lane loopcall): an `aload R; getfield F` site a loop
/// pre-header hoisted reads the slot, named by a synthetic local; every
/// other site decodes exactly as before.
#[test]
fn a_hoisted_field_site_decodes_as_its_synthetic_local() {
    let array_at_0 = |pc: usize| (pc == 0).then_some(HoistedFieldSite::Array(9));
    let int_at_0 = |pc: usize| (pc == 0).then_some(HoistedFieldSite::Int(9));
    let nothing = |_: usize| None;
    let field_7 = BoundSource::Field {
        cp_index: 7,
        receiver_local: Some(0),
    };
    // `aload_0; getfield #7; arraylength`, the array field hoisted.
    let len = [0x2a, 0xb4, 0x00, 0x07, 0xbe];
    assert_eq!(
        decode_bound_expr_with_hoisted_fields(&len, 0, 5, &no_math, &array_at_0),
        Some((BoundSource::ArrayLength(9), 5))
    );
    // Not hoisted: the historical reading, which stops at the `arraylength`
    // (so no exit test is ever read off this shape).
    assert_eq!(
        decode_bound_expr_with_hoisted_fields(&len, 0, 5, &no_math, &nothing),
        Some((field_7.clone(), 4))
    );
    assert_eq!(
        decode_bound_expr(&len, 0, 5, &no_math),
        Some((field_7.clone(), 4))
    );
    // The `arraylength` past `end` is not part of the expression.
    assert_eq!(
        decode_bound_expr_with_hoisted_fields(&len, 0, 4, &no_math, &array_at_0),
        Some((field_7.clone(), 4))
    );
    // An array site with no `arraylength` after it is not a length; a
    // hoisted `int` field is its slot's local.
    let size = [0x2a, 0xb4, 0x00, 0x07, 0x00];
    assert_eq!(
        decode_bound_expr_with_hoisted_fields(&size, 0, 5, &no_math, &array_at_0),
        Some((field_7.clone(), 4))
    );
    assert_eq!(
        decode_bound_expr_with_hoisted_fields(&size, 0, 4, &no_math, &int_at_0),
        Some((BoundSource::Local(9), 4))
    );
    // Keyed by the `aload`'s own pc: the same bytes one pc later are not
    // the hoisted site.
    let shifted = [0x00, 0x2a, 0xb4, 0x00, 0x07];
    assert_eq!(
        decode_bound_expr_with_hoisted_fields(&shifted, 1, 5, &no_math, &int_at_0),
        Some((field_7, 5))
    );
    // The two-byte `aload 5` receiver form.
    let recv5 = [0x19, 0x05, 0xb4, 0x00, 0x07];
    assert_eq!(
        decode_bound_expr_with_hoisted_fields(&recv5, 0, 5, &no_math, &int_at_0),
        Some((BoundSource::Local(9), 5))
    );
    // Inside `Math.min(16, this.size)`.
    let min = [0x10, 0x10, 0x2a, 0xb4, 0x00, 0x07, 0xb8, 0x00, 0x2a];
    let int_at_2 = |pc: usize| (pc == 2).then_some(HoistedFieldSite::Int(9));
    assert_eq!(
        decode_bound_expr_with_hoisted_fields(&min, 0, 9, &resolver, &int_at_2),
        Some((
            BoundSource::Min(
                Box::new(BoundSource::Const(16)),
                Box::new(BoundSource::Local(9))
            ),
            9
        ))
    );
}

/// `int sum() { int s = 0; for (int i = 0; i < data.length; i++) s += data[i]; return s; }`
/// (`this` = 0, `s` = 1, `i` = 2; the X2 fixture of `x64/tests.rs`). With
/// both `aload_0; getfield data` sites hoisted into one slot (synthetic
/// local 9) the loop is counted over `ArrayLength(9)`, and `data[i]` needs
/// no guard at all; without the hoist it is not a counted loop.
#[test]
fn a_hoisted_field_length_bound_is_a_counted_loop_over_the_slot() {
    let code: Vec<u8> = vec![
        0x03, 0x3c, 0x03, 0x3d, // 0: s = 0; i = 0
        0x1c, // 4: iload_2        <- header
        0x2a, 0xb4, 0x00, 0x01, 0xbe, // 5: aload_0; getfield data; arraylength
        0xa2, 0x00, 0x12, // 10: if_icmpge -> 28
        0x1b, 0x2a, 0xb4, 0x00, 0x01, 0x1c, 0x2e, 0x60, 0x3c, // 13: s += data[i]
        0x84, 0x02, 0x01, // 22: iinc 2, 1
        0xa7, 0xff, 0xeb, // 25: goto -> 4
        0x1b, 0xac, // 28: return s
    ];
    let t = LoopMethodTables::new(&code, code.len(), Some(&[]));
    assert!(
        t.analyze_counted_loop(4, 25, LoopForm::PreTested, &no_math)
            .is_none(),
        "unhoisted, `data.length` of a field is no limit the recogniser reads"
    );
    let hoisted = |pc: usize| matches!(pc, 5 | 14).then_some(HoistedFieldSite::Array(9));
    let l = t
        .analyze_counted_loop_with_hoisted_fields(
            4,
            25,
            LoopForm::PreTested,
            &no_math,
            &hoisted,
        )
        .expect("counted loop over the hoisted slot");
    assert_eq!(l.iv.local, 2);
    assert_eq!(l.iv.init, IntRange::constant(0));
    assert_eq!(l.cmp, ExitCmp::Ge);
    assert_eq!(l.bound, BoundSource::ArrayLength(9));
    assert_eq!(
        l.prove_index_in_bounds_of(
            &IndexExpr::identity(2),
            Some(9),
            IntRange::array_length(),
            &RangeEnv::new()
        ),
        BoundsProof::Static
    );
    // A different array (a real local) does not inherit the tautology.
    assert!(!matches!(
        l.prove_index_in_bounds_of(
            &IndexExpr::identity(2),
            Some(0),
            IntRange::array_length(),
            &RangeEnv::new()
        ),
        BoundsProof::Static
    ));
}

/// `for (int i = 0; i < size; i++) s += data[i];` with both fields hoisted:
/// `size` (pc 5) into synthetic local 10, `data` (pc 13) into 9. The limit
/// is the `int` slot's local, so `data[i]` needs the pre-header guard
/// `data.length >= size` -- which the emitter can now load -- and nothing
/// is static.
#[test]
fn a_hoisted_int_field_bound_is_the_slot_local() {
    let code: Vec<u8> = vec![
        0x03, 0x3c, 0x03, 0x3d, // 0: s = 0; i = 0
        0x1c, // 4: iload_2        <- header
        0x2a, 0xb4, 0x00, 0x02, // 5: aload_0; getfield size
        0xa2, 0x00, 0x12, // 9: if_icmpge -> 27
        0x1b, 0x2a, 0xb4, 0x00, 0x01, 0x1c, 0x2e, 0x60, 0x3c, // 12: s += data[i]
        0x84, 0x02, 0x01, // 21: iinc 2, 1
        0xa7, 0xff, 0xec, // 24: goto -> 4
        0x1b, 0xac, // 27: return s
    ];
    let t = LoopMethodTables::new(&code, code.len(), Some(&[]));
    let unhoisted = t
        .analyze_counted_loop(4, 24, LoopForm::PreTested, &no_math)
        .expect("a field limit is a counted loop");
    assert_eq!(
        unhoisted.bound,
        BoundSource::Field {
            cp_index: 2,
            receiver_local: Some(0)
        }
    );
    let hoisted = |pc: usize| match pc {
        5 => Some(HoistedFieldSite::Int(10)),
        13 => Some(HoistedFieldSite::Array(9)),
        _ => None,
    };
    let l = t
        .analyze_counted_loop_with_hoisted_fields(
            4,
            24,
            LoopForm::PreTested,
            &no_math,
            &hoisted,
        )
        .expect("counted loop over the hoisted limit");
    assert_eq!(l.bound, BoundSource::Local(10));
    assert_eq!(
        l.modified_locals & (1 << 10),
        0,
        "nothing stores a synthetic local"
    );
    assert!(matches!(
        l.prove_index_in_bounds_of(
            &IndexExpr::identity(2),
            Some(9),
            IntRange::array_length(),
            &RangeEnv::new()
        ),
        BoundsProof::Guarded(_)
    ));
}

#[test]
fn canonical_array_length_loop_is_proved_without_any_guard() {
    let code = array_length_loop();
    let l = analyze_counted_loop(&code, code.len(), 2, 11, LoopForm::PreTested, &no_math)
        .expect("counted loop");
    assert_eq!(l.iv.local, 1);
    assert_eq!(l.iv.stride, Stride::Const(1));
    assert_eq!(l.iv.init, IntRange::constant(0));
    assert_eq!(l.cmp, ExitCmp::Ge);
    assert_eq!(l.bound, BoundSource::ArrayLength(0));
    assert!(l.heap_stable);
    assert_eq!(l.modified_locals, 1 << 1);
    // Indexing the very array the limit came from needs nothing at all.
    assert_eq!(
        l.prove_index_in_bounds_of(
            &IndexExpr::identity(1),
            Some(0),
            IntRange::array_length(),
            &RangeEnv::new()
        ),
        BoundsProof::Static
    );
    // A different array does not inherit that proof.
    assert!(matches!(
        l.prove_index_in_bounds_of(
            &IndexExpr::identity(1),
            Some(3),
            IntRange::array_length(),
            &RangeEnv::new()
        ),
        BoundsProof::Guarded(_)
    ));
}

#[test]
fn inclusive_and_non_unit_and_negative_strides_are_all_recognised() {
    // for (i = 0; i <= n; i += 2)  — inclusive comparator, non-unit step.
    //  0: iconst_0   2: iload_1   4: if_icmpgt +10 → 14   11: goto -9 → 2
    //  1: istore_1   3: iload_2   8: iinc 1, 2            14: return
    let code = vec![
        0x03, 0x3c, 0x1b, 0x1c, 0xa3, 0x00, 0x0a, 0x00, 0x84, 0x01, 0x02, 0xa7, 0xff, 0xf7,
        0xb1,
    ];
    let l = analyze_counted_loop(&code, code.len(), 2, 11, LoopForm::PreTested, &no_math)
        .expect("counted loop");
    assert_eq!(l.cmp, ExitCmp::Gt);
    assert!(l.is_inclusive());
    assert_eq!(l.iv.stride, Stride::Const(2));
    assert_eq!(l.bound, BoundSource::Local(2));

    // for (i = n; i > 0; i--) — descending.
    //  0: iload_2  1: istore_1  2: iload_1  3: iconst_0  4: if_icmple +10 → 14
    //  7: nop      8: iinc 1,-1 11: goto -9 → 2          14: return
    let down = vec![
        0x1c, 0x3c, 0x1b, 0x03, 0xa4, 0x00, 0x0a, 0x00, 0x84, 0x01, 0xff, 0xa7, 0xff, 0xf7,
        0xb1,
    ];
    let d = analyze_counted_loop(&down, down.len(), 2, 11, LoopForm::PreTested, &no_math)
        .expect("counted loop");
    assert_eq!(d.cmp, ExitCmp::Le);
    assert_eq!(d.iv.stride, Stride::Const(-1));
    // The entry value came from another local, so it stays unknown — the
    // proof asks for a runtime witness rather than inventing one.
    assert!(d.iv.init.is_unknown());
}

#[test]
fn compound_assignment_strides_are_recognised_constant_and_variable() {
    // `j += 4` written as iload/iconst/iadd/istore.
    //  0: iload_1  1: iconst_4  2: iadd  3: istore_1
    let konst = vec![0x1b, 0x07, 0x60, 0x3c];
    assert_eq!(
        find_iv_stride(&konst, 0, konst.len(), 1),
        Some(Stride::Const(4))
    );
    // `j -= 4`.
    let minus = vec![0x1b, 0x07, 0x64, 0x3c];
    assert_eq!(
        find_iv_stride(&minus, 0, minus.len(), 1),
        Some(Stride::Const(-4))
    );
    // `j += step` — the Sieve inner loop; the step's sign is a runtime
    // fact, so it becomes a guarded stride, not an assumed positive one.
    let var = vec![0x1b, 0x1c, 0x60, 0x3c];
    assert_eq!(
        find_iv_stride(&var, 0, var.len(), 1),
        Some(Stride::Variable(2))
    );
    // MUST REFUSE: the commuted form is not distinguishable from an
    // unrelated expression overwriting the IV.
    let commuted = vec![0x1c, 0x1b, 0x60, 0x3c];
    assert_eq!(find_iv_stride(&commuted, 0, commuted.len(), 1), None);
    // MUST REFUSE: two modifications in one body.
    let twice = vec![0x84, 0x01, 0x01, 0x84, 0x01, 0x01];
    assert_eq!(find_iv_stride(&twice, 0, twice.len(), 1), None);
    // MUST REFUSE: a wide-indexed istore that aliases the IV.
    let wide = vec![0xc4, 0x36, 0x00, 0x01];
    assert_eq!(find_iv_stride(&wide, 0, wide.len(), 1), None);
    // MUST ACCEPT twin: the same wide istore on a different slot.
    let wide_other = vec![0xc4, 0x36, 0x00, 0x05, 0x84, 0x01, 0x01];
    assert_eq!(
        find_iv_stride(&wide_other, 0, wide_other.len(), 1),
        Some(Stride::Const(1))
    );
}

#[test]
fn a_non_int_store_to_the_iv_slot_refuses_the_loop() {
    // MUST REFUSE: `astore_1` reuses the IV's slot for a reference, which
    // the JVM permits across disjoint live ranges.
    let clobber = vec![0x84, 0x01, 0x01, 0x4c]; // iinc 1,1 ; astore_1
    assert_eq!(find_iv_stride(&clobber, 0, clobber.len(), 1), None);
    // MUST REFUSE: an `lstore_0` writes slot 1 as its dead high half.
    let high_half = vec![0x84, 0x01, 0x01, 0x3f]; // iinc 1,1 ; lstore_0
    assert_eq!(find_iv_stride(&high_half, 0, high_half.len(), 1), None);
    // MUST ACCEPT twin: the same store on a slot the IV does not occupy.
    let ok = vec![0x84, 0x01, 0x01, 0x4d]; // iinc 1,1 ; astore_2
    assert_eq!(find_iv_stride(&ok, 0, ok.len(), 1), Some(Stride::Const(1)));
}

#[test]
fn a_variable_stride_loop_carries_its_sign_guard_all_the_way_through() {
    // for (j = k; j < n; j += step) — locals: 1 = j, 2 = n, 3 = step.
    //  0: iload_1   1: iload_2   2: if_icmpge +12 → 14
    //  5: iload_1   6: iload_3   7: iadd   8: istore_1
    //  9: goto -9 → 0   12: nop  13: nop  14: return
    let code = vec![
        0x1b, 0x1c, 0xa2, 0x00, 0x0c, 0x1b, 0x1d, 0x60, 0x3c, 0xa7, 0xff, 0xf7, 0x00, 0x00,
        0xb1,
    ];
    let l = analyze_counted_loop(&code, code.len(), 0, 9, LoopForm::PreTested, &no_math)
        .expect("counted loop");
    assert_eq!(l.iv.stride, Stride::Variable(3));
    let p = l.prove_index_in_bounds(
        &IndexExpr::identity(1),
        IntRange::array_length(),
        &RangeEnv::new(),
    );
    match p {
        BoundsProof::Guarded(g) => {
            assert!(
                g.contains(&PreheaderGuard::StrideInRange {
                    local: 3,
                    headroom: SymBound {
                        base: crate::scev::BoundTerm::Bound(BoundSource::Local(2)),
                        addend: -1,
                    },
                }),
                "stride guard missing from {g:?}"
            );
        }
        other => panic!("expected Guarded, got {other:?}"),
    }
}

#[test]
fn a_loop_body_that_writes_the_limit_is_refused_by_the_proof() {
    // for (i = 0; i < n; i++) { n = something; } — the body rewrites the
    // limit local, so a pre-header guard on it would go stale.
    //  0: iconst_0  1: istore_1  2: iload_1  3: iload_2  4: if_icmpge +10
    //  7: iconst_0  8: istore_2  9: iinc 1,1  12: goto -10 → 2  15: return
    let code = vec![
        0x03, 0x3c, 0x1b, 0x1c, 0xa2, 0x00, 0x0b, 0x03, 0x3d, 0x84, 0x01, 0x01, 0xa7, 0xff,
        0xf6, 0xb1,
    ];
    let l = analyze_counted_loop(&code, code.len(), 2, 12, LoopForm::PreTested, &no_math)
        .expect("counted loop");
    assert_eq!(l.modified_locals & (1 << 2), 1 << 2);
    assert!(matches!(
        l.prove_index_in_bounds(
            &IndexExpr::identity(1),
            IntRange::array_length(),
            &RangeEnv::new()
        ),
        BoundsProof::Refused(_)
    ));
}

#[test]
fn modified_locals_strict_refuses_what_it_cannot_represent() {
    // A wide-indexed store above slot 63 cannot be recorded, so the whole
    // answer is refused instead of silently losing the write.
    let high = vec![0xc4, 0x36, 0x00, 0x64];
    assert_eq!(modified_locals_strict(&high, 0, high.len()), None);
    // A `long` store marks its dead high half too.
    let wide_pair = vec![0x40]; // lstore_1 → locals 1 and 2
    assert_eq!(
        modified_locals_strict(&wide_pair, 0, wide_pair.len()),
        Some(0b110)
    );
}

#[test]
fn heap_stability_is_required_for_a_field_bound() {
    assert!(body_is_heap_stable(&[0x1b, 0x84, 0x01, 0x01], 0, 4));
    // A call could change any field.
    assert!(!body_is_heap_stable(&[0xb6, 0x00, 0x01], 0, 3));
    // So could a putfield.
    assert!(!body_is_heap_stable(&[0xb5, 0x00, 0x01], 0, 3));
}

#[test]
fn constant_init_is_proved_or_left_unknown_never_assumed() {
    let code = array_length_loop();
    assert_eq!(constant_iv_init(&code, code.len(), 2, 14, 1), Some(0));
    // Local 0 is a parameter — never stored — so its entry value is
    // unknown, not zero.
    assert_eq!(constant_iv_init(&code, code.len(), 2, 14, 0), None);
}

/// `for (i = 0; i < 10; i++) { if (i == p) break; }`, or the same loop
/// with the `break` test replaced by `nop`s.
fn ten_trip_loop(with_break: bool) -> Vec<u8> {
    let mut code = vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1b, // 2: iload_1 (header)
        0x10, 10, // 3: bipush 10
        0xa2, 0x00, 14,   // 5: if_icmpge -> 19
        0x1b, // 8: iload_1
        0x1a, // 9: iload_0
        0x9f, 0x00, 9, // 10: if_icmpeq -> 19
        0x84, 1, 1, // 13: iinc 1, 1
        0xa7, 0xff, 0xf2, // 16: goto -> 2
        0xb1, // 19: return
    ];
    if !with_break {
        code[8..13].fill(0x00);
    }
    code
}

#[test]
fn a_second_exit_drops_the_trip_count_floor_to_zero() {
    let env = RangeEnv::default();
    let plain = analyze_counted_loop(
        &ten_trip_loop(false),
        20,
        2,
        16,
        LoopForm::PreTested,
        &no_math,
    )
    .expect("the plain loop is counted");
    assert!(!plain.has_other_exit);
    let t = plain.trip_count(&env).expect("constant trip count");
    assert_eq!((t.min, t.max), (10, 10));

    let broken = analyze_counted_loop(
        &ten_trip_loop(true),
        20,
        2,
        16,
        LoopForm::PreTested,
        &no_math,
    )
    .expect("the loop with a break is still counted");
    assert!(broken.has_other_exit);
    let t = broken.trip_count(&env).expect("an upper bound survives");
    assert_eq!((t.min, t.max), (0, 10));
    assert!(matches!(
        broken.prove_trip_count_at_least(5, &env),
        crate::scev::TripCountProof::Refused(_)
    ));
}

#[test]
fn an_enclosing_back_edge_between_the_store_and_the_header_unproves_the_init() {
    // for (;;) { i = 0 is outside; the outer loop re-enters at the inner
    // header without passing the store:
    // 0: iconst_0; 1: istore_1; 2: iload_1 (inner header); 3: bipush 10;
    // 5: if_icmpge -> 14; 8: iinc 1,1; 11: goto -> 2; 14: goto -> 2
    let mut code = vec![
        0x03, 0x3c, 0x1b, 0x10, 10, 0xa2, 0x00, 9, 0x84, 1, 1, 0xa7, 0xff, 0xf7, 0xa7, 0xff,
        0xf4, 0xb1,
    ];
    assert_eq!(constant_iv_init(&code, code.len(), 2, 14, 1), None);
    // Without the outer back edge the store runs before every entry.
    code[14..17].fill(0x00);
    assert_eq!(constant_iv_init(&code, code.len(), 2, 14, 1), Some(0));
}

// -- round 9 (irsched lane) --------------------------------------------

/// `void f(int[] a, int i, boolean c) { if (c) i = 0; for (; i < a.length;
/// i++) ... }` — `i` is a parameter, and the `ifeq` jumps to the header
/// PAST the only store. The entry value is not 0.
///
/// ```text
///  0: iload_2   1: ifeq -> 6   4: iconst_0   5: istore_1
///  6: iload_1 (header)   7: aload_0   8: arraylength   9: if_icmpge -> 18
/// 12: iinc 1,1   15: goto -> 6   18: return
/// ```
fn param_bypass_loop(with_bypass: bool) -> Vec<u8> {
    let mut code = vec![
        0x1c, 0x99, 0x00, 0x05, 0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01,
        0x01, 0xa7, 0xff, 0xf7, 0xb1,
    ];
    if !with_bypass {
        code[1..4].fill(0x00);
    }
    code
}

#[test]
fn a_forward_branch_past_the_init_store_unproves_the_constant_init() {
    let code = param_bypass_loop(true);
    assert_eq!(constant_iv_init(&code, code.len(), 6, 18, 1), None);
    let l = analyze_counted_loop(&code, code.len(), 6, 15, LoopForm::PreTested, &no_math)
        .expect("still a counted loop");
    assert!(
        l.iv.init.is_unknown(),
        "the entry value is a runtime quantity"
    );
    // MUST ACCEPT twin: without the bypass the store runs on every entry.
    let straight = param_bypass_loop(false);
    assert_eq!(
        constant_iv_init(&straight, straight.len(), 6, 18, 1),
        Some(0)
    );
}

/// javac's rotated loop: `goto cond` enters at the test, and the test is
/// the back edge, branching back into the body to CONTINUE.
///
/// ```text
///  0: iconst_0   1: istore_1   2: goto -> 8
///  5: iinc 1,1 (header)   8: iload_1   9: aload_0   10: arraylength
/// 11: if_icmplt -> 5   14: return
/// ```
fn rotated_loop() -> Vec<u8> {
    vec![
        0x03, 0x3c, 0xa7, 0x00, 0x06, 0x84, 0x01, 0x01, 0x1b, 0x2a, 0xbe, 0xa1, 0xff, 0xfa,
        0xb1,
    ]
}

#[test]
fn a_continue_branch_exit_test_is_negated_into_exit_polarity() {
    let code = rotated_loop();
    let (l, test_pc) =
        analyze_counted_loop_at(&code, code.len(), 5, 11, LoopForm::PreTested, &no_math)
            .expect("rotated counted loop");
    assert_eq!(test_pc, 8);
    // `if_icmplt body` continues while `i < a.length`: exits on `>=`.
    assert_eq!(l.cmp, ExitCmp::Ge);
    assert!(!l.is_inclusive());
    assert_eq!(l.iv.stride, Stride::Const(1));
    // The entry `goto` is the only way in, and it runs after the store.
    assert_eq!(l.iv.init, IntRange::constant(0));
}

#[test]
fn an_advance_inside_an_inner_loop_is_not_a_per_iteration_stride() {
    //  0: iload_1  1: iload_2  2: if_icmpge -> 15  5: iinc 1,1
    //  8: iload_3  9: ifne -> 5  12: goto -> 0  15: return
    let mut code = vec![
        0x1b, 0x1c, 0xa2, 0x00, 0x0d, 0x84, 0x01, 0x01, 0x1d, 0x9a, 0xff, 0xfc, 0xa7, 0xff,
        0xf4, 0xb1,
    ];
    assert_eq!(find_iv_stride(&code, 0, 15, 1), None);
    // MUST ACCEPT twin: without the inner back edge it is one advance.
    code[9..12].fill(0x00);
    assert_eq!(find_iv_stride(&code, 0, 15, 1), Some(Stride::Const(1)));
}

#[test]
fn an_advance_a_continue_can_skip_is_not_a_per_iteration_stride() {
    //  0: iload_1  1: iload_2  2: if_icmpge -> 15  5: iload_3
    //  6: ifeq -> 0 (skips the iinc)  9: iinc 1,1  12: goto -> 0  15: return
    let mut code = vec![
        0x1b, 0x1c, 0xa2, 0x00, 0x0d, 0x1d, 0x99, 0xff, 0xfa, 0x84, 0x01, 0x01, 0xa7, 0xff,
        0xf4, 0xb1,
    ];
    assert_eq!(find_iv_stride(&code, 0, 15, 1), None);
    // MUST ACCEPT twin: a branch forward onto the iinc (javac's `continue`).
    // 6: ifeq -> 9
    code[7] = 0x00;
    code[8] = 0x03;
    assert_eq!(find_iv_stride(&code, 0, 15, 1), Some(Stride::Const(1)));
}

// -- round 9 wave 2 (sched lane): exception-handler entries -------------

/// `for (i = 0; i < 10; i++) {}`, laid out pre-tested.
///
/// ```text
///  0: iconst_0   1: istore_1 (the init store)
///  2: iload_1 (header)   3: bipush 10   5: if_icmpge -> 14
///  8: iinc 1,1   11: goto -> 2   14: return
/// ```
fn counted_ten() -> Vec<u8> {
    vec![
        0x03, 0x3c, 0x1b, 0x10, 0x0a, 0xa2, 0x00, 0x09, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
        0xb1,
    ]
}

/// A handler inside the loop whose protected range covers code BEFORE the
/// init store enters the loop without the store having run.
#[test]
fn a_handler_that_bypasses_the_init_store_unproves_the_constant_init() {
    let code = counted_ten();
    let n = code.len();
    // Control: no handlers.
    assert_eq!(
        constant_iv_init_with_handlers(&code, n, 2, 14, 1, Some(&[])),
        Some(0)
    );
    // The handler at pc 8 protects `[0, 1)`: an exception there reaches
    // the body with the caller's `i`.
    assert_eq!(
        constant_iv_init_with_handlers(&code, n, 2, 14, 1, Some(&[(0, 1, 8)])),
        None
    );
    // MUST ACCEPT twin: a handler protecting only in-loop code runs after
    // the store.
    assert_eq!(
        constant_iv_init_with_handlers(&code, n, 2, 14, 1, Some(&[(2, 8, 8)])),
        Some(0)
    );
    // And the whole recognition refuses a loop an exception can enter
    // from outside.
    assert!(analyze_counted_loop_at_with_handlers(
        &code,
        n,
        2,
        11,
        LoopForm::PreTested,
        &no_math,
        Some(&[(0, 1, 8)]),
    )
    .is_none());
}

/// A handler that lands on the header from code ahead of the advance runs
/// an iteration that moves the IV by zero.
#[test]
fn a_handler_that_skips_the_advance_is_not_a_per_iteration_stride() {
    let code = counted_ten();
    let n = code.len();
    let at = |table: &[(usize, usize, usize)]| {
        analyze_counted_loop_at_with_handlers(
            &code,
            n,
            2,
            11,
            LoopForm::PreTested,
            &no_math,
            Some(table),
        )
    };
    // `[3, 8)` precedes the `iinc` at 8 and its handler is the header.
    assert!(at(&[(3, 8, 2)][..]).is_none());
    // MUST ACCEPT twin: the same range handled at pc 5, ahead of the
    // advance, still runs it (javac's in-loop `catch` shape).
    let (l, test_pc) = at(&[(3, 5, 5)][..]).expect("in-loop catch: still counted");
    assert_eq!(test_pc, 2);
    assert_eq!(l.iv.stride, Stride::Const(1));
    assert_eq!(l.iv.init, IntRange::constant(0));
    // A range AFTER the advance handled at or before it makes the advance
    // part of an inner loop.
    assert!(at(&[(11, 14, 8)][..]).is_none());
}

/// With no exception table, code no normal edge reaches is a possible
/// handler for anything, so a loop that contains some is refused -- and
/// the same loop with the real (empty) table is accepted.
///
/// ```text
///  0: iconst_0   1: istore_1   2: iload_1 (header)   3: bipush 10
///  5: if_icmpge -> 18   8: goto -> 12   11: nop (unreachable)
/// 12: iinc 1,1   15: goto -> 2   18: return
/// ```
#[test]
fn without_a_table_unreachable_code_in_the_loop_is_a_possible_handler() {
    let code: Vec<u8> = vec![
        0x03, 0x3c, 0x1b, 0x10, 0x0a, 0xa2, 0x00, 0x0d, 0xa7, 0x00, 0x04, 0x00, 0x84, 0x01,
        0x01, 0xa7, 0xff, 0xf3, 0xb1,
    ];
    let n = code.len();
    assert!(
        analyze_counted_loop_at(&code, n, 2, 15, LoopForm::PreTested, &no_math).is_none(),
        "no table: the unreachable pc 11 may be a handler entry"
    );
    let (l, _) = analyze_counted_loop_at_with_handlers(
        &code,
        n,
        2,
        15,
        LoopForm::PreTested,
        &no_math,
        Some(&[]),
    )
    .expect("the real table says there is no handler");
    assert_eq!(l.iv.init, IntRange::constant(0));
    // The synthetic entries are exactly the unreachable instruction.
    assert_eq!(handler_entries(&code, n, None), vec![(0, n, 11)]);
}

/// Round 11: `for (int i = 0; i < a.length; i = back ? i - 1 : i + 1)
/// a[i] = 7;` — javac's output, whose `istore` is a join of an `isub` arm
/// and an `iadd` arm. The textual tail `iload i; iconst_1; iadd; istore i`
/// read as `+1`, and BCE removed `a[i]`'s check for a loop that walks
/// below zero.
///
/// ```text
///  0: iconst_0  1: istore_2  2: iload_2  3: aload_0  4: arraylength
///  5: if_icmpge -> 30  8: aload_0  9: iload_2  10: bipush 7  12: iastore
/// 13: iload_1  14: ifeq -> 23  17: iload_2  18: iconst_1  19: isub
/// 20: goto -> 26  23: iload_2  24: iconst_1  25: iadd  26: istore_2
/// 27: goto -> 2  30: return
/// ```
#[test]
fn a_ternary_advance_is_not_a_constant_stride() {
    let mut code: Vec<u8> = vec![
        0x03, 0x3d, 0x1c, 0x2a, 0xbe, 0xa2, 0x00, 0x19, 0x2a, 0x1c, 0x10, 0x07, 0x4f, 0x1b,
        0x99, 0x00, 0x09, 0x1c, 0x04, 0x64, 0xa7, 0x00, 0x06, 0x1c, 0x04, 0x60, 0x3d, 0xa7,
        0xff, 0xe7, 0xb1,
    ];
    assert_eq!(code.len(), 31);
    assert_eq!(find_iv_stride(&code, 2, 30, 2), None);
    // MUST ACCEPT twin: no ternary, so nothing lands inside the advance.
    code[13..23].fill(0x00);
    assert_eq!(find_iv_stride(&code, 2, 30, 2), Some(Stride::Const(1)));
}

/// Round 11: javac reuses one slot for sibling loops.
///
/// ```text
///  0: iconst_0  1: istore_1  2: iload_1  3: aload_0  4: arraylength
///  5: if_icmpge -> 14  8: iinc 1,1  11: goto -> 2
/// 14: iconst_0  15: istore_1  16: iload_1  17: aload_0  18: arraylength
/// 19: if_icmpge -> 28  22: iinc 1,1  25: goto -> 16  28: return
/// ```
///
/// Two out-of-body stores (and each loop's `iinc` outside the other) made
/// BOTH entry values unknown; and the first loop's exit lands on the
/// second init's push.
#[test]
fn sibling_loops_sharing_a_slot_each_keep_their_constant_init() {
    let code: Vec<u8> = vec![
        0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
        0x03, 0x3c, 0x1b, 0x2a, 0xbe, 0xa2, 0x00, 0x09, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xf7,
        0xb1,
    ];
    let n = code.len();
    assert_eq!(n, 29);
    assert_eq!(constant_iv_init(&code, n, 2, 14, 1), Some(0));
    assert_eq!(constant_iv_init(&code, n, 16, 28, 1), Some(0));
    // A branch landing on the STORE (past the push) still refuses: turn
    // the first loop's exit into `if_icmpge -> 15`.
    let mut skip_push = code.clone();
    skip_push[7] = 0x0a;
    assert_eq!(constant_iv_init(&skip_push, n, 16, 28, 1), None);
}

/// Round 11: a rotated `while ((c ? i : j) < a.length) { a[j] = 0; j++; }`
/// (ecj's layout). The `iload j` triple is entered at the bound by the `i`
/// arm's `goto`, so it is not the test of `j`.
///
/// ```text
///  0: iconst_0  1: istore_3  2: goto -> 12
///  5: aload_0  6: iload_3  7: iconst_0  8: iastore  9: iinc 3,1
/// 12: iload_1  13: ifeq -> 20  16: iload_2  17: goto -> 21
/// 20: iload_3  21: aload_0  22: arraylength  23: if_icmplt -> 5
/// 26: return
/// ```
#[test]
fn an_exit_test_entered_past_its_iload_is_not_that_locals_test() {
    let mut code: Vec<u8> = vec![
        0x03, 0x3e, 0xa7, 0x00, 0x0a, 0x2a, 0x1d, 0x03, 0x4f, 0x84, 0x03, 0x01, 0x1b, 0x99,
        0x00, 0x07, 0x1c, 0xa7, 0x00, 0x04, 0x1d, 0x2a, 0xbe, 0xa1, 0xff, 0xee, 0xb1,
    ];
    assert_eq!(code.len(), 27);
    let n = code.len();
    assert!(
        analyze_counted_loop_at(&code, n, 5, 23, LoopForm::PreTested, &no_math).is_none(),
        "the tested value is `c ? i : j`, not `j`"
    );
    // MUST ACCEPT twin: `while (j < a.length)`.
    code[12..20].fill(0x00);
    let (l, test_pc) = analyze_counted_loop_at(&code, n, 5, 23, LoopForm::PreTested, &no_math)
        .expect("plain rotated counted loop");
    assert_eq!(test_pc, 20);
    assert_eq!(l.iv.local, 3);
    assert_eq!(l.iv.stride, Stride::Const(1));
}
