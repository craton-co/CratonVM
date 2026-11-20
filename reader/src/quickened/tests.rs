use super::*;

fn padded(mut v: Vec<u8>) -> Arc<[u8]> {
    v.push(0);
    v.push(0);
    Arc::from(v.into_boxed_slice())
}

/// Ground truth: the exact set of `(pc, insn, next_pc)` the interpreter's
/// original `Instruction::decode` walk produces, computed independently
/// of `QuickenedCode`.
fn linear_walk(code: &Arc<[u8]>) -> Vec<(usize, Instruction, usize)> {
    let limit = code.len() - 2;
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < limit {
        match Instruction::decode(code, pc) {
            Ok((insn, next)) if next > pc => {
                out.push((pc, insn, next));
                pc = next;
            }
            _ => break,
        }
    }
    out
}

/// A method body exercising every shape that matters: single- and
/// multi-byte instructions, a `wide` prefix, a `tableswitch` and a
/// `lookupswitch` (both with alignment padding), and a back-edge.
fn mixed_method() -> Arc<[u8]> {
    let mut b: Vec<u8> = Vec::new();
    b.push(0x03); // 0: iconst_0
    b.push(0x3c); // 1: istore_1
    b.push(0xc4); // 2: wide iload 300
    b.push(0x15);
    b.extend(300u16.to_be_bytes());
    b.push(0xc4); // 6: wide iinc 300, 5
    b.push(0x84);
    b.extend(300u16.to_be_bytes());
    b.extend(5i16.to_be_bytes());
    b.push(0x10); // 12: bipush 7
    b.push(0x07);
    b.push(0x60); // 14: iadd
    b.push(0x3c); // 15: istore_1
                  // 16: tableswitch (pc 16, next byte 17 -> pad to 20)
    b.push(0xaa);
    while b.len() % 4 != 0 {
        b.push(0);
    }
    b.extend(40i32.to_be_bytes()); // default
    b.extend(0i32.to_be_bytes()); // low
    b.extend(2i32.to_be_bytes()); // high  -> 3 offsets
    b.extend(4i32.to_be_bytes());
    b.extend(8i32.to_be_bytes());
    b.extend(12i32.to_be_bytes());
    // lookupswitch
    b.push(0xab);
    while b.len() % 4 != 0 {
        b.push(0);
    }
    b.extend(16i32.to_be_bytes()); // default
    b.extend(2i32.to_be_bytes()); // npairs
    b.extend(100i32.to_be_bytes());
    b.extend(20i32.to_be_bytes());
    b.extend(200i32.to_be_bytes());
    b.extend(24i32.to_be_bytes());
    b.push(0x1b); // iload_1
    b.push(0xa7); // goto -2 (back-edge)
    b.extend((-2i16).to_be_bytes());
    b.push(0xb1); // return
    padded(b)
}

#[test]
fn pc_block_is_sixteen_bytes() {
    assert_eq!(std::mem::size_of::<PcBlock>(), 16);
    assert_eq!(std::mem::align_of::<PcBlock>(), 8);
}

#[test]
fn quickened_matches_decode_at_every_pc() {
    // iconst_0; istore_1; iload_1; bipush 7; iadd; istore_1; return
    let code = padded(vec![0x03, 0x3c, 0x1b, 0x10, 0x07, 0x60, 0x3c, 0xb1]);
    let q = QuickenedCode::build(&code).expect("quickenable");
    let mut pc = 0usize;
    let mut idx = 0usize;
    while pc < code.len() - 2 {
        let (expect_insn, expect_next) = Instruction::decode(&code, pc).unwrap();
        let i = q.index_of_pc(pc, idx).expect("pc is an instruction start");
        assert_eq!(i, idx);
        assert_eq!(q.pc_of(i), pc);
        assert_eq!(*q.op(i), expect_insn);
        assert_eq!(q.next_pc(i), expect_next);
        pc = expect_next;
        idx = i + 1;
    }
    assert_eq!(q.len(), idx);
}

/// The core O(1) requirement: for a method containing `wide`,
/// `tableswitch` and `lookupswitch`, resolving an *arbitrary* pc must
/// agree exactly with the independent linear walk — both for the pcs
/// that are instruction starts and for every interior byte that is not.
#[test]
fn random_access_pc_lookup_matches_linear_walk() {
    let code = mixed_method();
    let truth = linear_walk(&code);
    assert!(truth.len() > 8, "fixture should be non-trivial");
    let q = QuickenedCode::build(&code).expect("quickenable");
    assert!(q.has_dense_index());
    assert_eq!(q.len(), truth.len());

    // Every byte offset in the method, probed out of order and with a
    // deliberately wrong hint every time.
    let limit = code.len() - 2;
    for pc in (0..limit).rev() {
        let expected = truth.iter().position(|(p, _, _)| *p == pc);
        for hint in [0usize, 1, 3, q.len(), q.len() + 7, usize::MAX] {
            assert_eq!(
                q.index_of_pc(pc, hint),
                expected,
                "pc={pc} hint={hint} disagrees with the linear walk"
            );
        }
        assert_eq!(q.index_of_pc_direct(pc), expected, "pc={pc} (hintless)");
        match expected {
            Some(i) => {
                let (insn, next) = q.resolve(pc).expect("start resolves");
                assert_eq!(*insn, truth[i].1, "pc={pc} instruction");
                assert_eq!(next, truth[i].2, "pc={pc} next_pc");
                assert_eq!(q.pc_of(i), pc);
                assert_eq!(q.next_pc(i), truth[i].2);
            }
            None => assert!(q.resolve(pc).is_none(), "pc={pc} must not resolve"),
        }
    }
    // Past the end of the method is never a start.
    assert_eq!(q.index_of_pc_direct(limit + 1), None);
    assert_eq!(q.index_of_pc_direct(usize::MAX), None);
}

#[test]
fn wide_prefixed_instruction_is_one_record() {
    // wide iload 300; wide iinc 300, 5; return
    let mut b = vec![0xc4, 0x15];
    b.extend(300u16.to_be_bytes());
    b.push(0xc4);
    b.push(0x84);
    b.extend(300u16.to_be_bytes());
    b.extend(5i16.to_be_bytes());
    b.push(0xb1);
    let code = padded(b);
    let q = QuickenedCode::build(&code).unwrap();
    assert_eq!(q.len(), 3);
    assert_eq!(*q.op(0), Instruction::Iload(300));
    assert_eq!(q.pc_of(0), 0);
    assert_eq!(q.next_pc(0), 4);
    assert_eq!(
        *q.op(1),
        Instruction::Iinc {
            index: 300,
            constant: 5
        }
    );
    assert_eq!(q.pc_of(1), 4);
    assert_eq!(q.next_pc(1), 10);
    assert_eq!(*q.op(2), Instruction::Return);
    // Interior bytes of both wide instructions are not starts.
    for pc in [1usize, 2, 3, 5, 6, 7, 8, 9] {
        assert_eq!(q.index_of_pc_direct(pc), None, "interior pc={pc}");
    }
}

#[test]
fn interior_pc_is_not_an_instruction_start() {
    // bipush 7 occupies pc 0..2; pc 1 is its operand byte.
    let code = padded(vec![0x10, 0x07, 0xb1]);
    let q = QuickenedCode::build(&code).unwrap();
    assert_eq!(q.index_of_pc(0, 0), Some(0));
    assert_eq!(q.index_of_pc(1, 0), None);
    assert_eq!(q.index_of_pc(1, 1), None);
    assert_eq!(q.index_of_pc(2, 1), Some(1));
}

#[test]
fn wrong_hint_still_resolves() {
    let code = padded(vec![0x03, 0x3c, 0x1b, 0x10, 0x07, 0x60, 0x3c, 0xb1]);
    let q = QuickenedCode::build(&code).unwrap();
    for i in 0..q.len() {
        let pc = q.pc_of(i);
        // Every possible (including nonsensical) hint must agree.
        for hint in 0..q.len() + 3 {
            assert_eq!(q.index_of_pc(pc, hint), Some(i), "pc={pc} hint={hint}");
        }
        assert_eq!(q.index_of_pc(pc, usize::MAX), Some(i));
    }
}

#[test]
fn switch_tables_are_interned_out_of_line() {
    // tableswitch at pc 0 with 3-byte alignment padding.
    let mut body = vec![0xaa, 0, 0, 0];
    body.extend(&12i32.to_be_bytes()); // default
    body.extend(&0i32.to_be_bytes()); // low
    body.extend(&1i32.to_be_bytes()); // high
    body.extend(&20i32.to_be_bytes()); // offsets[0]
    body.extend(&24i32.to_be_bytes()); // offsets[1]
    let code = padded(body);
    let q = QuickenedCode::build(&code).unwrap();
    match q.op(0) {
        Instruction::Tableswitch(t) => {
            assert_eq!(t.low, 0);
            assert_eq!(t.high, 1);
            assert_eq!(t.offsets, vec![20, 24]);
        }
        other => panic!("expected tableswitch, got {other:?}"),
    }
    // Executing a switch must not clone the table: the record is a
    // fixed-size `Instruction` regardless of table size.
    assert!(std::mem::size_of::<Instruction>() <= 16);
}

/// Both switch flavours must land in the stream as borrowed, interned
/// payloads that repeated lookups hand back without re-decoding.
#[test]
fn both_switch_kinds_resolve_without_reallocating() {
    let code = mixed_method();
    let q = QuickenedCode::build(&code).unwrap();
    let mut table = None;
    let mut lookup = None;
    for i in 0..q.len() {
        match q.op(i) {
            Instruction::Tableswitch(t) => table = Some((i, Arc::as_ptr(t))),
            Instruction::Lookupswitch(l) => lookup = Some((i, Arc::as_ptr(l))),
            _ => {}
        }
    }
    let (ti, tptr) = table.expect("fixture has a tableswitch");
    let (li, lptr) = lookup.expect("fixture has a lookupswitch");
    // Re-resolving by pc yields the *same* payload allocation every time.
    for _ in 0..4 {
        let (insn, _) = q.resolve(q.pc_of(ti)).unwrap();
        match insn {
            Instruction::Tableswitch(t) => assert!(std::ptr::eq(Arc::as_ptr(t), tptr)),
            other => panic!("expected tableswitch, got {other:?}"),
        }
        let (insn, _) = q.resolve(q.pc_of(li)).unwrap();
        match insn {
            Instruction::Lookupswitch(l) => assert!(std::ptr::eq(Arc::as_ptr(l), lptr)),
            other => panic!("expected lookupswitch, got {other:?}"),
        }
    }
}

#[test]
fn unquickenable_code_returns_none() {
    // 0xba (invokedynamic) needs 4 operand bytes; truncating mid-operand
    // makes the linear walk fail.
    let code: Arc<[u8]> = Arc::from(vec![0xba, 0x00].into_boxed_slice());
    assert!(QuickenedCode::build(&code).is_none());
    // A first instruction that cannot decode at all is also not quickened.
    let bad = padded(vec![0xfe, 0xb1]);
    assert!(QuickenedCode::build(&bad).is_none());
}

/// A decode failure partway through salvages the decodable prefix, so a
/// switch before the bad bytes still dispatches from the interned stream
/// instead of re-decoding (and re-allocating) on every execution.
#[test]
fn decode_failure_salvages_the_prefix() {
    let mut b = vec![0xaa, 0, 0, 0];
    b.extend(&12i32.to_be_bytes()); // default
    b.extend(&0i32.to_be_bytes()); // low
    b.extend(&1i32.to_be_bytes()); // high
    b.extend(&20i32.to_be_bytes()); // offsets[0]
    b.extend(&24i32.to_be_bytes()); // offsets[1]
    let bad_pc = b.len();
    b.push(0xfe); // undefined opcode: the walk stops here
    b.push(0x03);
    let code = padded(b);
    let q = QuickenedCode::build(&code).expect("prefix is salvageable");
    assert_eq!(q.len(), 1);
    assert!(matches!(q.op(0), Instruction::Tableswitch(_)));
    assert_eq!(q.index_of_pc_direct(0), Some(0));
    assert_eq!(q.next_pc(0), bad_pc);
    // The undecodable pc reports "not found" so the caller re-decodes
    // there and fails at exactly the pc it always did.
    assert_eq!(q.index_of_pc_direct(bad_pc), None);
    assert!(Instruction::decode(&code, bad_pc).is_err());
}

/// A method too large for the dense index falls back to binary search and
/// must stay exactly as correct.
#[test]
fn oversized_method_falls_back_to_binary_search() {
    // One byte past the JVMS code_length ceiling, so the highest start pc
    // exceeds MAX_DENSE_START_PC.
    let n = MAX_DENSE_START_PC + 2;
    let mut body = vec![0x00u8; n - 1]; // nops
    body.push(0xb1); // return
    let code = padded(body);
    let q = QuickenedCode::build(&code).expect("quickenable");
    assert!(
        !q.has_dense_index(),
        "a start pc past the ceiling must not build a dense index"
    );
    assert_eq!(q.index_bytes(), 0);
    assert_eq!(q.len(), n);
    // Still exact, via the binary-search path.
    for pc in [0usize, 1, 63, 64, 65_534, 65_535, n - 1] {
        assert_eq!(q.index_of_pc_direct(pc), Some(pc), "pc={pc}");
        assert_eq!(q.pc_of(pc), pc);
    }
    assert_eq!(*q.op(n - 1), Instruction::Return);
    assert_eq!(q.index_of_pc_direct(n), None);
    assert_eq!(q.index_of_pc_direct(usize::MAX), None);

    // The largest method that *does* qualify keeps the dense index.
    let mut body = vec![0x00u8; MAX_DENSE_START_PC];
    body.push(0xb1);
    let code = padded(body);
    let q = QuickenedCode::build(&code).expect("quickenable");
    assert!(q.has_dense_index());
    assert_eq!(
        q.index_of_pc_direct(MAX_DENSE_START_PC),
        Some(MAX_DENSE_START_PC)
    );
    assert_eq!(*q.op(MAX_DENSE_START_PC), Instruction::Return);
}

/// Multi-block methods must carry the cumulative counts correctly across
/// block boundaries — the part a single-block fixture cannot exercise.
#[test]
fn cumulative_counts_span_blocks() {
    // 200 bipush (2 bytes each) = 400 bytes, spanning 7 blocks, so every
    // start lands at an even pc and every odd pc is an operand byte.
    let mut body = Vec::new();
    for _ in 0..200 {
        body.push(0x10);
        body.push(0x2a);
    }
    let code = padded(body);
    let q = QuickenedCode::build(&code).unwrap();
    assert!(q.has_dense_index());
    assert_eq!(q.len(), 200);
    for i in 0..200 {
        assert_eq!(q.index_of_pc_direct(i * 2), Some(i), "start pc={}", i * 2);
        assert_eq!(
            q.index_of_pc_direct(i * 2 + 1),
            None,
            "operand pc={}",
            i * 2 + 1
        );
    }
}

#[test]
fn heap_bytes_accounts_for_the_dense_index() {
    let code = mixed_method();
    let q = QuickenedCode::build(&code).unwrap();
    assert!(q.index_bytes() > 0);
    assert_eq!(q.index_bytes() % std::mem::size_of::<PcBlock>(), 0);
    let floor = q.index_bytes() + q.len() * std::mem::size_of::<Instruction>();
    assert!(
        q.heap_bytes() >= floor,
        "heap_bytes must include the dense index"
    );
}

/// The census's fall-through lengths agree with the decoder for every
/// opcode whose length the opcode alone fixes, and name the three
/// variable-length ones 0.
#[test]
fn fixed_instruction_length_matches_decode() {
    let mut checked = 0;
    for op in 0u8..=0xc9 {
        let len = fixed_instruction_length(op);
        if matches!(op, 0xaa | 0xab | 0xc4) {
            assert_eq!(len, 0, "{} is variable-length", opcode_mnemonic(op));
            continue;
        }
        assert!(len > 0, "{} has a fixed length", opcode_mnemonic(op));
        // Operands of 1 keep every index / count / dimension decodable.
        let mut body = vec![op];
        body.extend([1u8; 8]);
        if let Ok((_, next)) = Instruction::decode(&padded(body), 0) {
            assert_eq!(next, usize::from(len), "{}", opcode_mnemonic(op));
            checked += 1;
        }
    }
    assert!(checked > 190, "only {checked} opcodes decoded");
    assert_eq!(fixed_instruction_length(0xca), 0, "unassigned");
    assert_eq!(opcode_mnemonic(0x1a), "iload_0");
    assert_eq!(opcode_mnemonic(0xc9), "jsr_w");
    assert_eq!(opcode_mnemonic(0xca), "?");
}

/// Recorded pairs come back from the census, most frequent first.
#[test]
fn opcode_pair_census_counts_and_ranks() {
    // Unassigned opcodes: nothing else in the process records them.
    for _ in 0..3 {
        record_opcode_pair(0xfd, 0xfe);
    }
    record_opcode_pair(0xfd, 0xff);
    let top = opcode_pair_census_top(PAIR_SLOTS);
    let pos = |a: u8, b: u8| top.iter().position(|r| r.0 == a && r.1 == b);
    let three = pos(0xfd, 0xfe).expect("recorded");
    let one = pos(0xfd, 0xff).expect("recorded");
    assert!(top[three].2 >= 3 && top[one].2 >= 1);
    assert!(three < one, "ranked by count");
}

/// Interpreter round i6 wave 120, lane L8: recorded triples come back
/// from the census (the recording thread's map is flushed by the query),
/// most frequent first.
#[test]
fn opcode_triple_census_counts_and_ranks() {
    // Unassigned opcodes: nothing else in the process records them.
    for _ in 0..5 {
        record_opcode_triple(0xfd, 0xfe, 0xff);
    }
    record_opcode_triple(0xff, 0xfe, 0xfd);
    let top = opcode_triple_census_top(usize::MAX);
    let pos =
        |a: u8, b: u8, c: u8| top.iter().position(|r| r.0 == a && r.1 == b && r.2 == c);
    let five = pos(0xfd, 0xfe, 0xff).expect("recorded");
    let one = pos(0xff, 0xfe, 0xfd).expect("recorded");
    assert!(top[five].3 >= 5 && top[one].3 >= 1);
    assert!(five < one, "ranked by count");
}

#[test]
fn intern_returns_the_same_stream_for_the_same_allocation() {
    let code = padded(vec![0x03, 0xb1]);
    let a = intern(&code).unwrap();
    let b = intern(&code).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    assert!(Arc::ptr_eq(a.code(), &code));
}

/// `fast_path_local_reach`: short, explicit and `wide` forms count their
/// index plus their width; a category-2 access counts two slots.
#[test]
fn local_reach_counts_every_local_access_form() {
    let reach = |v: Vec<u8>| fast_path_local_reach(&padded(v), []);
    assert_eq!(reach(vec![0x03, 0xac]), Some(0), "no locals");
    assert_eq!(reach(vec![0x1d, 0xac]), Some(4), "iload_3");
    assert_eq!(reach(vec![0x09, 0x42, 0xb1]), Some(5), "lstore_3 reaches 4");
    assert_eq!(reach(vec![0x18, 0x06, 0xaf]), Some(8), "dload 6 reaches 7");
    assert_eq!(reach(vec![0x84, 0x07, 0x01, 0xb1]), Some(8), "iinc 7");
    // wide iload 300; ireturn
    assert_eq!(reach(vec![0xc4, 0x15, 0x01, 0x2c, 0xac]), Some(301));
}

/// Control that can leave the linear walk makes the reach unknown.
#[test]
fn local_reach_refuses_code_control_can_leave() {
    // iload_0; ifeq +4 (-> 5); iconst_1; ireturn — a branch to a start.
    let ok = vec![0x1a, 0x99, 0x00, 0x04, 0x04, 0xac];
    assert_eq!(fast_path_local_reach(&padded(ok.clone()), []), Some(1));
    assert_eq!(fast_path_local_reach(&padded(ok.clone()), [4]), Some(1));
    assert_eq!(
        fast_path_local_reach(&padded(ok), [2]),
        None,
        "a handler inside an instruction"
    );
    let refused = |v: Vec<u8>| fast_path_local_reach(&padded(v), []).is_none();
    // bipush 0x1d; goto -1 — lands on the operand byte, which is iload_3.
    assert!(
        refused(vec![0x10, 0x1d, 0xa7, 0xff, 0xff]),
        "into an operand"
    );
    assert!(refused(vec![0xa7, 0xff, 0xfb]), "goto -5 at pc 0");
    assert!(refused(vec![0xa9, 0x00]), "ret: a runtime target");
    // iload_0; <undefined opcode>; ireturn: the walk stops early.
    assert!(refused(vec![0x1a, 0xcb, 0xac]), "a truncated walk");
}

/// A pool with a `long` field `A.f` (#6) and a static method
/// `A.m(IJLjava/lang/String;)D` (#10).
fn member_pool() -> crate::constant_pool::ConstantPool {
    use crate::constant_pool::ConstantPoolEntry as E;
    let utf8 = |s: &str| E::Utf8(s.into());
    crate::constant_pool::ConstantPool::new(vec![
        E::Tombstone,
        utf8("f"),
        utf8("J"),
        E::NameAndType {
            name_index: 1,
            descriptor_index: 2,
        },
        E::ClassReference { name_index: 5 },
        utf8("A"),
        E::FieldReference {
            class_index: 4,
            name_and_type_index: 3,
        },
        utf8("m"),
        utf8("(IJLjava/lang/String;)D"),
        E::NameAndType {
            name_index: 7,
            descriptor_index: 8,
        },
        E::MethodReference {
            class_index: 4,
            name_and_type_index: 9,
        },
    ])
}

/// Wave 7: `fast_path_reach` proves the operand-stack depth in words —
/// member descriptors from the pool, loops at a consistent depth,
/// handlers entered at depth 1 — and refuses (`stack: None`, locals still
/// proven) an underflow, a join at two depths, an unresolvable member and
/// code that falls off its end.
#[test]
fn stack_reach_is_proven_in_words_or_refused() {
    let cp = member_pool();
    let stack = |v: Vec<u8>, handlers: Vec<usize>| {
        fast_path_reach(&padded(v), handlers, Some(&cp)).and_then(|r| r.stack)
    };
    // getstatic #6 (J); lconst_1; ladd; putstatic #6; iconst_0; lconst_0;
    // aconst_null; invokestatic #10 (IJL)D; dreturn
    let members = vec![
        0xb2, 0, 6, 0x0a, 0x61, 0xb3, 0, 6, 0x03, 0x09, 0x01, 0xb8, 0, 10, 0xaf,
    ];
    assert_eq!(stack(members, vec![]), Some(4));
    // iconst_0; istore_0; L: iload_0; bipush 10; if_icmpge +9; iinc 0 1;
    // goto L (-9); return
    let lp = vec![
        0x03, 0x3b, 0x1a, 0x10, 10, 0xa2, 0, 9, 0x84, 0, 1, 0xa7, 0xff, 0xf7, 0xb1,
    ];
    assert_eq!(stack(lp, vec![]), Some(2));
    // iconst_0; ireturn; handler 2: astore_0; aload_0; athrow
    assert_eq!(stack(vec![0x03, 0xac, 0x4b, 0x2a, 0xbf], vec![2]), Some(1));
    assert_eq!(stack(vec![0x57, 0xb1], vec![]), None, "pop of nothing");
    // iconst_0; ifeq +5 (-> 6, depth 0); iconst_1; nop; return (depth 1)
    let join = vec![0x03, 0x99, 0, 5, 0x04, 0x00, 0xb1];
    assert_eq!(stack(join.clone(), vec![]), None, "a join at two depths");
    assert_eq!(fast_path_local_reach(&padded(join), []), Some(0));
    assert_eq!(
        stack(vec![0xb2, 0, 99, 0x57, 0xb1], vec![]),
        None,
        "no member #99"
    );
    assert_eq!(stack(vec![0x03], vec![]), None, "falls off the end");
    assert_eq!(
        fast_path_reach(&padded(vec![0x03, 0xac]), [], None).map(|r| r.stack),
        Some(None),
        "no pool, no stack proof"
    );
    assert_eq!(method_descriptor_words("()J"), Some((0, 2)));
    assert_eq!(
        method_descriptor_words("(IJ[DLjava/lang/String;[[Ljava/lang/Object;)V"),
        Some((6, 0))
    );
    assert_eq!(method_descriptor_words("(V)V"), None);
}

/// An obsolete body's `ldc` whose constant sits above index 255 is
/// decoded as `ldc_w` of that index at its own pc; every other record,
/// and every pc, is the plain build's. A pc that is not an `ldc`, and a
/// body the walk cannot finish, are refused.
#[test]
fn widened_ldc_streams_replace_only_the_named_records() {
    // 0: ldc #4; 2: ldc #5; 4: pop; 5: pop; 6: return
    let code = padded(vec![0x12, 4, 0x12, 5, 0x57, 0x57, 0xb1]);
    let q = QuickenedCode::with_widened_ldc(&code, &[(2, 300)]).expect("widens");
    assert_eq!(q.resolve(0), Some((&Instruction::Ldc(4), 2)));
    assert_eq!(q.resolve(2), Some((&Instruction::LdcW(300), 4)));
    assert_eq!(q.resolve(4), Some((&Instruction::Pop, 5)));
    assert_eq!(q.len(), QuickenedCode::build(&code).map_or(0, |b| b.len()));
    assert!(
        QuickenedCode::with_widened_ldc(&code, &[(4, 300)]).is_none(),
        "not an ldc"
    );
    assert!(
        QuickenedCode::with_widened_ldc(&code, &[(1, 300)]).is_none(),
        "not an instruction start"
    );
    // ldc #4, then an undecodable opcode: the walk stops short.
    let truncated = padded(vec![0x12, 4, 0xff, 0xb1]);
    assert!(QuickenedCode::with_widened_ldc(&truncated, &[(0, 300)]).is_none());
}
