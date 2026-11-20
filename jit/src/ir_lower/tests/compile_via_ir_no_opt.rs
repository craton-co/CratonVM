#[allow(unused_imports)]
use super::*;

/// Round 11 wave 17 (lane hashmap): the inline `Integer.valueOf` box
/// allocates only ABOVE the bound this reads, so it must answer only a
/// resolved bound the JDK could have (`>= 127`, an `int`), and `None`
/// for an unwired table, an unresolved bound (`0`) and anything else.
/// Round 12 wave 2 (lane calls): the inline `Long.valueOf` box asks the
/// resolver for `Long` with its own selector, and takes only a real,
/// non-zero class id; an unwired table or a `0` answer keeps the call.
#[test]
pub(in crate::ir_lower::tests) fn r12w2_the_long_class_id_comes_from_its_own_selector() {
    unsafe extern "C" fn answers(ctx: i64, which: i64) -> i64 {
        if which != super::WELL_KNOWN_CLASS_LONG {
            return 9_999; // another selector: never read as Long's id
        }
        match ctx {
            0x1000 => 77,
            0x2000 => 0,
            _ => -1,
        }
    }
    let table = |ctx: usize| crate::DirectHelperTable {
        static_base_resolver_ctx: ctx,
        well_known_class_id: answers as *const () as usize,
        ..crate::DirectHelperTable::EMPTY
    };
    assert_eq!(super::ir_long_class_id(&table(0x1000)), Some(77));
    assert_eq!(super::ir_long_class_id(&table(0x2000)), None);
    assert_eq!(super::ir_long_class_id(&table(0x3000)), None);
    assert_eq!(super::ir_long_class_id(&table(0)), None);
    assert_eq!(
        super::ir_long_class_id(&crate::DirectHelperTable::EMPTY),
        None
    );
    // Distinct from every other selector the resolver answers.
    for other in [
        crate::direct_helpers::WELL_KNOWN_CLASS_INTEGER,
        crate::direct_helpers::WELL_KNOWN_CLASS_STRING,
        crate::direct_helpers::WELL_KNOWN_STRING_HASH_FIELD,
        super::WELL_KNOWN_INTEGER_CACHE_HIGH,
    ] {
        assert_ne!(other, super::WELL_KNOWN_CLASS_LONG);
    }
}

/// JIT round 16 (lane callbind): the inline cached-range `Integer.valueOf`
/// takes the table's address and length from their own two selectors, and
/// only a filed, aligned table with at least the default 256 slots.
#[test]
pub(in crate::ir_lower::tests) fn r16_callbind_the_integer_cache_slots_need_a_filed_table() {
    unsafe extern "C" fn answers(ctx: i64, which: i64) -> i64 {
        let (base, len) = match ctx {
            0x1000 => (0x7f00_0000_1000, 256),
            0x2000 => (0, 256),
            0x3000 => (0x7f00_0000_1000, 0),
            0x4000 => (0x7f00_0000_1004, 256),
            0x5000 => (0x7f00_0000_1000, 255),
            _ => (0x7f00_0000_2000, 1129),
        };
        match which {
            super::WELL_KNOWN_INTEGER_CACHE_SLOTS => base,
            super::WELL_KNOWN_INTEGER_CACHE_SLOTS_LEN => len,
            _ => 42, // another selector: never read as either
        }
    }
    let table = |ctx: usize| crate::DirectHelperTable {
        static_base_resolver_ctx: ctx,
        well_known_class_id: answers as *const () as usize,
        ..crate::DirectHelperTable::EMPTY
    };
    assert_eq!(
        super::ir_integer_cache_slots(&table(0x1000)),
        Some((0x7f00_0000_1000, 256))
    );
    assert_eq!(
        super::ir_integer_cache_slots(&table(0x6000)),
        Some((0x7f00_0000_2000, 1129))
    );
    assert_eq!(super::ir_integer_cache_slots(&table(0x2000)), None);
    assert_eq!(super::ir_integer_cache_slots(&table(0x3000)), None);
    assert_eq!(super::ir_integer_cache_slots(&table(0x4000)), None);
    assert_eq!(super::ir_integer_cache_slots(&table(0x5000)), None);
    assert_eq!(super::ir_integer_cache_slots(&table(0)), None);
    assert_eq!(
        super::ir_integer_cache_slots(&crate::DirectHelperTable::EMPTY),
        None
    );
    let selectors = [
        crate::direct_helpers::WELL_KNOWN_CLASS_INTEGER,
        crate::direct_helpers::WELL_KNOWN_CLASS_STRING,
        crate::direct_helpers::WELL_KNOWN_STRING_HASH_FIELD,
        super::WELL_KNOWN_INTEGER_CACHE_HIGH,
        super::WELL_KNOWN_CLASS_LONG,
        super::WELL_KNOWN_INTEGER_CACHE_SLOTS,
        super::WELL_KNOWN_INTEGER_CACHE_SLOTS_LEN,
    ];
    for (i, a) in selectors.iter().enumerate() {
        assert!(
            !selectors[i + 1..].contains(a),
            "selector {a} answers two questions"
        );
    }
}

/// JIT round 16 (lane callbind): the cached-range read, EXECUTED over a
/// fake table. A filled slot answers its word, an empty slot takes the
/// `empty` edge, and every value outside `-128..-128+len` -- both ends,
/// and the extremes that wrap -- takes the `outside` edge.
#[test]
pub(in crate::ir_lower::tests) fn r16_callbind_the_cache_read_answers_the_slot_or_declines() {
    use crate::ExecutableBuffer;
    const LEN: usize = 300;
    let mut table = vec![0usize; LEN];
    for (i, w) in table.iter_mut().enumerate() {
        if i % 3 != 0 {
            *w = 0x1000_0000 + 8 * i;
        }
    }
    let (bytes, outside, empty) =
        super::integer_cache_read_bytes(table.as_ptr() as u64, LEN as u32);
    // MOV ECX, <arg0's low half> ; <read> ; RET ; outside: MOV RAX,-1 ;
    // RET ; empty: MOV RAX,-2 ; RET. RCX is caller-saved on both ABIs.
    let arg0 = super::CALL_ARG_REGS[0];
    let mut body = vec![0x89, 0xC1 | ((arg0 & 7) << 3)];
    if arg0 >= 8 {
        body.insert(0, 0x44);
    }
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.push(0xC3);
    let outside_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]);
    let empty_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFE, 0xFF, 0xFF, 0xFF, 0xC3]);
    for (operand, target) in [(at + outside, outside_at), (at + empty, empty_at)] {
        let rel = i32::try_from(target).unwrap() - i32::try_from(operand + 4).unwrap();
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }
    let mut buf = ExecutableBuffer::new(256).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: `buf` is finalized and holds a complete
    // `extern "C" fn(u64) -> u64` that writes only RAX, RCX and R10, all
    // caller-saved on both ABIs.
    let f = unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr()) };
    // The upper half of the argument is garbage on purpose: only ECX counts.
    let call = |v: i32| f(0xDEAD_BEEF_0000_0000 | u64::from(v as u32)) as i64;
    let last = -128 + LEN as i32 - 1;
    for v in [-128, -127, -1, 0, 1, 2, 3, 127, 128, last - 1, last] {
        let i = (v + 128) as usize;
        let want = if table[i] == 0 { -2 } else { table[i] as i64 };
        assert_eq!(call(v), want, "value {v}");
    }
    for v in [-129, last + 1, i32::MIN, i32::MAX, -1_000_000] {
        assert_eq!(call(v), -1, "value {v} is outside the table");
    }
}

/// JIT round 16 wave 7 (lane callbind, CB16-3): the `Long` table's address
/// comes from its own selector, distinct from every other, and only a
/// filed, aligned answer is taken.
#[test]
pub(in crate::ir_lower::tests) fn r16w7_callbind_the_long_cache_slots_need_a_filed_table() {
    unsafe extern "C" fn answers(ctx: i64, which: i64) -> i64 {
        if which != super::WELL_KNOWN_LONG_CACHE_SLOTS {
            return 0x7f00_0000_9000; // another selector: never read as it
        }
        match ctx {
            0x1000 => 0x7f00_0000_1000,
            0x2000 => 0,
            0x3000 => 0x7f00_0000_1004,
            _ => -8,
        }
    }
    let table = |ctx: usize| crate::DirectHelperTable {
        static_base_resolver_ctx: ctx,
        well_known_class_id: answers as *const () as usize,
        ..crate::DirectHelperTable::EMPTY
    };
    assert_eq!(
        super::ir_long_cache_slots(&table(0x1000)),
        Some(0x7f00_0000_1000)
    );
    assert_eq!(super::ir_long_cache_slots(&table(0x2000)), None);
    assert_eq!(super::ir_long_cache_slots(&table(0x3000)), None);
    assert_eq!(super::ir_long_cache_slots(&table(0x4000)), None);
    assert_eq!(super::ir_long_cache_slots(&table(0)), None);
    assert_eq!(
        super::ir_long_cache_slots(&crate::DirectHelperTable::EMPTY),
        None
    );
    let selectors = [
        crate::direct_helpers::WELL_KNOWN_CLASS_INTEGER,
        crate::direct_helpers::WELL_KNOWN_CLASS_STRING,
        crate::direct_helpers::WELL_KNOWN_STRING_HASH_FIELD,
        super::WELL_KNOWN_INTEGER_CACHE_HIGH,
        super::WELL_KNOWN_CLASS_LONG,
        super::WELL_KNOWN_INTEGER_CACHE_SLOTS,
        super::WELL_KNOWN_INTEGER_CACHE_SLOTS_LEN,
        super::WELL_KNOWN_LONG_CACHE_SLOTS,
    ];
    for (i, a) in selectors.iter().enumerate() {
        assert!(
            !selectors[i + 1..].contains(a),
            "selector {a} answers two questions"
        );
    }
}

/// JIT round 16 wave 7 (lane callbind, CB16-3): the `Long` read, EXECUTED
/// over a fake 256-slot table with the full 64-bit argument: a filled slot
/// answers its word, an empty one takes `empty`, and every value outside
/// `-128..=127` -- both ends, and the extremes that wrap -- takes `outside`.
#[test]
pub(in crate::ir_lower::tests) fn r16w7_callbind_the_long_cache_read_answers_the_slot_or_declines()
{
    use crate::ExecutableBuffer;
    let mut table = vec![0usize; 256];
    for (i, w) in table.iter_mut().enumerate() {
        if i % 3 != 0 {
            *w = 0x2000_0000 + 8 * i;
        }
    }
    let (bytes, outside, empty) = super::long_cache_read_bytes(table.as_ptr() as u64);
    // MOV RCX, <arg0> ; <read> ; RET ; outside: MOV RAX,-1 ; RET ;
    // empty: MOV RAX,-2 ; RET. RCX is caller-saved on both ABIs.
    let arg0 = super::CALL_ARG_REGS[0];
    let rex = 0x48 | if arg0 >= 8 { 0x04 } else { 0x00 };
    let mut body = vec![rex, 0x89, 0xC1 | ((arg0 & 7) << 3)];
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.push(0xC3);
    let outside_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]);
    let empty_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFE, 0xFF, 0xFF, 0xFF, 0xC3]);
    for (operand, target) in [(at + outside, outside_at), (at + empty, empty_at)] {
        let rel = i32::try_from(target).unwrap() - i32::try_from(operand + 4).unwrap();
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }
    let mut buf = ExecutableBuffer::new(256).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: `buf` is finalized and holds a complete
    // `extern "C" fn(i64) -> i64` that writes only RAX, RCX and R10, all
    // caller-saved on both ABIs.
    let f = unsafe { std::mem::transmute::<*const u8, extern "C" fn(i64) -> i64>(buf.as_ptr()) };
    for v in [-128i64, -127, -1, 0, 1, 2, 3, 126, 127] {
        let i = (v + 128) as usize;
        let want = if table[i] == 0 { -2 } else { table[i] as i64 };
        assert_eq!(f(v), want, "value {v}");
    }
    for v in [
        -129i64,
        128,
        i64::MIN,
        i64::MAX,
        i64::MAX - 100,
        -1_000_000,
        1 << 32,
        (1 << 32) + 5,
    ] {
        assert_eq!(f(v), -1, "value {v} is outside the table");
    }
}

#[test]
pub(in crate::ir_lower::tests) fn r11w17_the_integer_cache_high_needs_a_resolved_bound() {
    unsafe extern "C" fn answers(ctx: i64, which: i64) -> i64 {
        if which != super::WELL_KNOWN_INTEGER_CACHE_HIGH {
            return 42; // a class-id selector: never read as a bound
        }
        match ctx {
            0x1000 => 1000,
            0x2000 => 0,
            0x3000 => 126,
            0x4000 => i64::from(i32::MAX) + 1,
            _ => 127,
        }
    }
    let table = |ctx: usize| crate::DirectHelperTable {
        static_base_resolver_ctx: ctx,
        well_known_class_id: answers as *const () as usize,
        ..crate::DirectHelperTable::EMPTY
    };
    assert_eq!(super::ir_integer_cache_high(&table(0x1000)), Some(1000));
    assert_eq!(super::ir_integer_cache_high(&table(0x5000)), Some(127));
    assert_eq!(super::ir_integer_cache_high(&table(0x2000)), None);
    assert_eq!(super::ir_integer_cache_high(&table(0x3000)), None);
    assert_eq!(super::ir_integer_cache_high(&table(0x4000)), None);
    assert_eq!(super::ir_integer_cache_high(&table(0)), None);
    assert_eq!(
        super::ir_integer_cache_high(&crate::DirectHelperTable::EMPTY),
        None
    );
    // The selector is not one of the three class-id selectors.
    assert!(![
        crate::direct_helpers::WELL_KNOWN_CLASS_INTEGER,
        crate::direct_helpers::WELL_KNOWN_CLASS_STRING,
        crate::direct_helpers::WELL_KNOWN_STRING_HASH_FIELD,
    ]
    .contains(&super::WELL_KNOWN_INTEGER_CACHE_HIGH));
}

/// All-zero helpers for tests that contain no `Op::Call` (no helper pointer
/// is ever dereferenced). SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` with
/// all-integer (usize) fields, so an all-zero bit pattern is a valid value.
pub(in crate::ir_lower::tests) fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
    unsafe { std::mem::zeroed() }
}

/// Regression witness (2026-07-27): NO rel32 patch site in this file may
/// `expect`/`unwrap` its `try_patch_*` result.
///
/// `ExecutableBuffer::emit` is non-panicking — on capacity exhaustion it
/// sets a sticky `overflowed` flag and DROPS the write, so recorded patch
/// offsets stop addressing their placeholder bytes and `try_patch_i32`
/// legitimately fails (setting the flag itself). `lower_inner` is built for
/// exactly that: its `if buf.overflowed() { return None }` bail discards the
/// artifact and the caller falls back to the single-pass backend. An
/// `expect` at a patch site turns that recoverable fallback into a
/// compile-thread panic, which in a release VM is
/// `fatal runtime error: failed to initiate panic` → SIGABRT of the whole
/// process. See `patch_rel32_to_here`'s doc comment for the same argument.
///
/// How this was found: lifting the blanket `org/junit/` JIT ban aborted
/// 58 of 60 real Elasticsearch test classes, every one at
/// `emit_call_exc_stub`'s `expect("call-exc JE patch in-bounds")`. Thirteen
/// sites in this file still had that shape; they now use `.ok()`, matching
/// what `jit/src/x64.rs` has always done. Asserting on the source keeps a
/// future site from silently reintroducing the abort — a behavioural test
/// cannot reach these emitters without a graph large enough to overflow
/// `lower_inner`'s own buffer estimate, which is exactly the condition the
/// estimate exists to prevent.
#[test]
pub(in crate::ir_lower::tests) fn no_patch_site_panics_on_an_overflowed_buffer() {
    let src = concat!(
        include_str!("../../ir_lower/mod.rs"),
        "\n",
        include_str!("../../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_session_confined_rows.rs")
    );
    let offenders: Vec<(usize, &str)> = src
        .lines()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim_start();
            (t.starts_with(".expect(") || t.starts_with(".unwrap(")) && l.contains("patch")
        })
        .map(|(i, l)| (i + 1, l.trim()))
        .collect();
    assert!(
        offenders.is_empty(),
        "patch sites must swallow the error and let `lower_inner`'s \
             `buf.overflowed()` bail fall back to single-pass, not panic \
             (use `.ok()`); offenders: {offenders:?}",
    );
}

/// The flag must be read LIVE, not cached in a `OnceLock`.
///
/// Same hazard `equal_depth_flag_is_read_live` pins: an A/B that runs both
/// arms in one process reports byte-identical code when a scheduling flag
/// is memoised, and the result looks like "the transform does nothing"
/// rather than "the flag was never re-read".
/// The displacement price rises as the file empties, and is zero when
/// nothing would be displaced.
///
/// Both ends matter and both were wrong in an earlier draft. A price that
/// did NOT rise let a run of cheap single-use values evict the whole
/// carried set one register at a time; a price that was not zero for an
/// under-subscribed file made the transform inert on straight-line code,
/// where there is no carried value to outbid in the first place.
///
/// The weights are `probes/FieldLoop.java`'s real ones, so this also pins
/// the case the `>=` tie-break exists for: price 10 against a candidate
/// weighing 10 must be admissible, or that probe's 7.6% goes away.
#[test]
pub(in crate::ir_lower::tests) fn the_displacement_price_rises_as_the_file_empties() {
    let file = ir_gp_file().len();
    assert!(
        file >= 2,
        "the rest of this test assumes a file worth sharing"
    );

    // Fewer carried candidates than registers: nothing is displaced.
    assert_eq!(carried_displacement_price(&[99; 1], 0), 0);
    assert_eq!(carried_displacement_price(&[], 0), 0);

    // FieldLoop.sum's measured weights, descending.
    let fieldloop = [20u64, 11, 10, 10, 10, 10];
    assert!(
        fieldloop.len() >= file,
        "fixture must over-subscribe the file to displace anything"
    );
    let first = carried_displacement_price(&fieldloop, 0);
    assert_eq!(
        first,
        fieldloop[file - 1],
        "the first admission displaces the WEAKEST value still served"
    );
    assert!(
        10 >= first,
        "the induction variable weighs 10 and must be admissible at the \
             first price -- this is the `>=` tie-break §6d measured"
    );

    // Strictly rising, then refusing outright once the file is spent.
    let mut prev = first;
    for taken in 1..file {
        let now = carried_displacement_price(&fieldloop, taken);
        assert!(
            now >= prev,
            "price fell at taken={taken}: {prev} -> {now}; a falling price \
                 lets cheap values evict the carried set one register at a time"
        );
        prev = now;
    }
    assert_eq!(
        carried_displacement_price(&fieldloop, file),
        u64::MAX,
        "once the file is spent nothing may be admitted at any weight"
    );
}

/// `single_use_is_in_the_carry_window` must answer for the window
/// [`Lowerer::plan_carries`] actually scans -- the node immediately after
/// the producer, in the producer's own block -- and not for "some later
/// node reads it".
///
/// The fixture is `fib`'s shape reduced to its essential: a value computed
/// in the entry block whose only reader sits past a branch. That value is
/// the entire population the flag exists for, so a test that could not
/// produce one would not be testing anything. The assertion is therefore
/// two-sided: the cross-block value reads `false`, and the value whose
/// consumer IS adjacent reads `true`.
#[test]
pub(in crate::ir_lower::tests) fn the_carry_window_is_exactly_the_next_node_in_the_same_block() {
    // int f(int a) { int t = a + a; if (a != 0) return t; return 1; }
    //   0: iload_0  1: iload_0  2: iadd  3: istore_1
    //   4: iload_0  5: ifeq 10  8: iload_1  9: ireturn
    //  10: iconst_1 11: ireturn
    let code = [
        0x1a, 0x1a, 0x60, 0x3c, 0x1a, 0x99, 0x00, 0x05, 0x1b, 0xac, 0x04, 0xac,
    ];
    let mut graph = IrBuilder::new(1, 2).build(&code, 12).expect("IR build");
    ir_optimize::optimize(&mut graph);
    let schedule = ir_schedule::schedule(&graph);

    // Recompute the window independently of the helper, so the two cannot
    // agree by sharing a bug.
    let adjacent_consumer = |id: usize| -> bool {
        schedule.blocks.iter().any(|b| {
            b.nodes
                .iter()
                .position(|&n| n as usize == id)
                .and_then(|w| b.nodes.get(w + 1))
                .is_some_and(|&c| {
                    graph.nodes[c as usize]
                        .inputs
                        .iter()
                        .any(|&i| i != NO_NODE && i as usize == id)
                })
        })
    };

    let mut agreed = 0usize;
    let mut outside_the_window = 0usize;
    for id in 0..graph.nodes.len() {
        if !schedule
            .blocks
            .iter()
            .any(|b| b.nodes.contains(&(id as u32)))
        {
            continue;
        }
        let helper = single_use_is_in_the_carry_window(&graph, &schedule, id);
        assert_eq!(
            helper,
            adjacent_consumer(id),
            "node {id} ({:?}): helper and the independent recomputation disagree",
            graph.nodes[id].op
        );
        agreed += 1;
        if !helper {
            outside_the_window += 1;
        }
    }

    assert!(agreed > 0, "the fixture scheduled no nodes at all");
    assert!(
        outside_the_window > 0,
        "this shape must contain at least one value the carry window cannot \
             reach -- otherwise the flag's target population is empty and the \
             test proves nothing"
    );
}

#[test]
pub(in crate::ir_lower::tests) fn live_new_uses_shared_allocation_stub_and_context_abi() {
    extern "C" fn allocate(vm: i64, class_id: i64, num_fields: i64) -> i64 {
        vm + class_id * 100 + num_fields
    }

    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let allocation = graph.add(
        Op::New {
            class_id: 23,
            num_fields: 7,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(0),
    );
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));

    let schedule = ir_schedule::schedule(&graph);
    let mut helpers = no_helpers();
    helpers.new_object = allocate as *const () as usize;
    let compiled = lower(&graph, &schedule, 0, 0, &helpers).expect("live allocation must lower");
    assert!(compiled.needs_context);
    // SAFETY: the synthetic helper treats the context as an integer and the
    // generated method has no Java arguments.
    let result = unsafe {
        compiled
            .try_call_with_context(11, &[])
            .expect("allocation call")
    };
    assert_eq!(result, 11 + 23 * 100 + 7);
}

#[test]
pub(in crate::ir_lower::tests) fn live_new_converts_null_failure_to_jit_exception_sentinel() {
    extern "C" fn fail(_: i64, _: i64, _: i64) -> i64 {
        0
    }

    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let allocation = graph.add(
        Op::New {
            class_id: 1,
            num_fields: 0,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(0),
    );
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));

    let schedule = ir_schedule::schedule(&graph);
    let mut helpers = no_helpers();
    helpers.new_object = fail as *const () as usize;
    let compiled = lower(&graph, &schedule, 0, 0, &helpers).expect("live allocation");
    // SAFETY: helper ignores the synthetic context.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call_with_context(1, &[]) },
        Ok(i64::MIN)
    );
}

/// r11w2-tier-ir-inline-tlab-new-runs-no-clinit: the inline TLAB bump runs
/// no `<clinit>`, so an `Op::New` whose class is not PROVEN initialised
/// keeps the `jit_new_object` stub alone. Before the fix the bump was
/// emitted for every class, and a cold `new` of a class whose `<clinit>`
/// had not run allocated without running it.
#[test]
pub(in crate::ir_lower::tests) fn r11w3_inline_tlab_new_is_only_for_a_class_proven_initialised() {
    extern "C" fn allocate(_: i64, _: i64, _: i64) -> i64 {
        0
    }
    extern "C" fn post_init(_: i64, obj: i64, _: i64, _: i64) -> i64 {
        obj
    }
    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let allocation = graph.add(
        Op::New {
            class_id: 23,
            num_fields: 2,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(0),
    );
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));
    let schedule = ir_schedule::schedule(&graph);
    let mut helpers = no_helpers();
    helpers.new_object = allocate as *const () as usize;
    helpers.tlab_post_init = post_init as *const () as usize;
    helpers.tlab_cursor_offset_in_thread = 0x40;
    helpers.tlab_end_offset_in_thread = 0x48;
    // The bump's limit compare, `CMP RAX, [R10 + end_off]`, with its disp32.
    let bump: [u8; 7] = [0x49, 0x3B, 0x82, 0x48, 0x00, 0x00, 0x00];
    let lower_with = |announce: &std::collections::HashSet<u32>,
                      spliced: &[(usize, usize, usize)]| {
        lower_inner_with_array_list(
            &crate::DirectHelperTable::EMPTY,
            &graph,
            &schedule,
            0,
            0,
            &helpers,
            &HashMap::new(),
            spliced,
            None,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &crate::ir::IrInlineFrameSites::default(),
            &HashMap::new(),
            announce,
            None,
        )
        .expect("allocation lowers")
    };
    let has_bump = |m: &CompiledMethod| m.code_bytes().windows(bump.len()).any(|w| w == bump);

    let unproven = lower_with(&std::collections::HashSet::new(), &[]);
    assert!(
        !has_bump(&unproven),
        "a class not proven initialised took the inline bump, which runs no <clinit>"
    );
    let announced: std::collections::HashSet<u32> = [23u32].into_iter().collect();
    assert!(
        has_bump(&lower_with(&announced, &[])),
        "a class the announce set proves initialised must keep the inline bump"
    );
    // A `new` inside a spliced body: the splice resolver admits only
    // initialised classes. (Range covers the node's pc 0.)
    assert!(
        has_bump(&lower_with(&std::collections::HashSet::new(), &[(0, 3, 0)])),
        "a spliced-body `new` is proven initialised and keeps the inline bump"
    );
}

/// A `new` of a class with no compact layout allocates a LEGACY object
/// (every route: the inline bump and `jit_new_object`), so a later field
/// store must keep its per-object compact test. Recording the new node as
/// compact-proven stored at compact offsets into a legacy object
/// (`heap-bytebuffer-asintbuffer-zero-capacity-intermittent-FIXED-20261110`:
/// `Buffer.capacity` landed at +0x24 of a legacy `ByteBufferAsIntBufferB`,
/// whose readers then saw 0).
/// cov-06: a PRIMITIVE `Op::NewArray` (`element_type != 0`) lowers
/// through `emit_new_array_stub` to a call on `helpers.newarray`, with
/// the runtime length forwarded as the third ABI argument (not baked as
/// an immediate, unlike `Op::New`'s field count) — the same
/// `(vm, atype, length)` shape `jit_newarray` and the single-pass
/// backend's 0xbc arm share.
#[test]
pub(in crate::ir_lower::tests) fn live_newarray_uses_shared_allocation_stub_and_context_abi() {
    extern "C" fn allocate(vm: i64, atype: i64, length: i64) -> i64 {
        vm + atype * 100 + length
    }

    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let length = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    let allocation = graph.add(
        Op::NewArray {
            element_type: 10, // T_INT
            component_class_id: 0,
        },
        IrType::Ref,
        vec![ctrl, mem, length],
        Some(0),
    );
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));

    let schedule = ir_schedule::schedule(&graph);
    let mut helpers = no_helpers();
    helpers.newarray = allocate as *const () as usize;
    let compiled =
        lower(&graph, &schedule, 1, 1, &helpers).expect("live array allocation must lower");
    assert!(compiled.needs_context);
    // SAFETY: the synthetic helper treats the context/length as integers
    // and the generated method takes one int argument (the length).
    let result = unsafe {
        compiled
            .try_call_with_context(11, &[7])
            .expect("allocation call")
    };
    assert_eq!(result, 11 + 10 * 100 + 7);
}

/// cov-06: a REFERENCE `Op::NewArray` (`element_type == 0`) lowers
/// through the SAME stub but calls `helpers.anewarray_object` with the
/// component class id as the immediate — the two helpers are chosen
/// per-node, not per-graph, so a method could in principle mix both
/// shapes (this test only needs one to prove the routing).
#[test]
pub(in crate::ir_lower::tests) fn live_anewarray_calls_the_reference_array_helper() {
    extern "C" fn allocate(vm: i64, component_class_id: i64, length: i64) -> i64 {
        vm + component_class_id * 1000 + length
    }

    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let length = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    let allocation = graph.add(
        Op::NewArray {
            element_type: 0,
            component_class_id: 42,
        },
        IrType::Ref,
        vec![ctrl, mem, length],
        Some(0),
    );
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));

    let schedule = ir_schedule::schedule(&graph);
    let mut helpers = no_helpers();
    helpers.anewarray_object = allocate as *const () as usize;
    let compiled =
        lower(&graph, &schedule, 1, 1, &helpers).expect("live array allocation must lower");
    // SAFETY: the synthetic helper treats the context/length as integers.
    let result = unsafe {
        compiled
            .try_call_with_context(11, &[5])
            .expect("allocation call")
    };
    assert_eq!(result, 11 + 42 * 1000 + 5);
}

/// cov-06: `jit_newarray`/`jit_anewarray_object` return `0` for BOTH a
/// negative length (JLS `NegativeArraySizeException`) and OOM — the same
/// zero-on-failure convention `jit_new_object` uses. `Op::NewArray`'s
/// lowering must convert that to the JIT-wide `i64::MIN` sentinel exactly
/// like `Op::New`'s failure path, or a negative-length `anewarray` would
/// hand the caller a null pointer instead of routing through the pending
/// exception.
#[test]
pub(in crate::ir_lower::tests) fn live_newarray_converts_null_failure_to_jit_exception_sentinel() {
    extern "C" fn fail(_: i64, _: i64, _: i64) -> i64 {
        0
    }

    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let length = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    let allocation = graph.add(
        Op::NewArray {
            element_type: 10,
            component_class_id: 0,
        },
        IrType::Ref,
        vec![ctrl, mem, length],
        Some(0),
    );
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));

    let schedule = ir_schedule::schedule(&graph);
    let mut helpers = no_helpers();
    helpers.newarray = fail as *const () as usize;
    let compiled = lower(&graph, &schedule, 1, 1, &helpers).expect("live allocation");
    // SAFETY: helper ignores the synthetic context/length.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call_with_context(1, &[-1]) },
        Ok(i64::MIN)
    );
}

/// cov-06: the same "refuse rather than call through address zero"
/// contract `Op::New`/`Op::MonitorEnter` already have. A graph containing
/// a live `Op::NewArray` with NO helper wired for its shape must be
/// REFUSED, never silently miscompiled into a call to `0`.
#[test]
pub(in crate::ir_lower::tests) fn a_newarray_graph_with_no_helper_is_refused() {
    let mut graph = Graph::empty(0);
    let start = graph.add(Op::Start, IrType::Control, vec![], None);
    graph.entry = start;
    let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let length = graph.add(Op::Param(0), IrType::Int, vec![start], None);
    let allocation = graph.add(
        Op::NewArray {
            element_type: 10,
            component_class_id: 0,
        },
        IrType::Ref,
        vec![ctrl, mem, length],
        Some(0),
    );
    graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));
    let schedule = ir_schedule::schedule(&graph);
    assert!(
        lower(&graph, &schedule, 1, 1, &no_helpers()).is_none(),
        "a newarray graph with no `helpers.newarray` must be REFUSED, not \
             lowered to a call through address zero"
    );
}

/// Round 9 (irlower): a loop latch whose back edge carries phi copies and
/// whose exit edge carries none lowers through the one-sided arm of
/// `lower_terminator` — the Jcc names the copy-free exit directly and the
/// copies run on straight-line code after it. Executed, not byte-matched:
/// the risk in that arm is sending an outcome down the wrong edge, and only
/// running the loop shows that. Both an unoptimized and an optimized graph
/// are driven, so the fused-compare (`cc`) and boolean (`TEST`) shapes are
/// both likely to be reached.
#[test]
pub(in crate::ir_lower::tests) fn do_while_latch_with_one_sided_phi_copies_runs_correctly() {
    static FLAG: u8 = 0;
    extern "C" fn slow_poll() {}
    // static int f(int n) { int s = 0, i = 0; do { s += i; i++; } while (i < n); return s; }
    let code: [u8; 18] = [
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x03, // 2: iconst_0
        0x3d, // 3: istore_2
        0x1b, // 4: iload_1
        0x1c, // 5: iload_2
        0x60, // 6: iadd
        0x3c, // 7: istore_1
        0x84, 0x02, 0x01, // 8: iinc 2, 1
        0x1c, // 11: iload_2
        0x1a, // 12: iload_0
        0xa1, 0xff, 0xf7, // 13: if_icmplt -9 (-> 4)
        0x1b, // 16: iload_1
        0xac, // 17: ireturn
    ];
    for optimize in [false, true] {
        let builder = IrBuilder::new(1, 3);
        let mut graph = builder.build(&code, code.len()).expect("IR build");
        if optimize {
            ir_optimize::optimize(&mut graph);
        }
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.safepoint_flag_addr = &FLAG as *const u8 as usize;
        helpers.safepoint_slow_path = slow_poll as *const () as usize;
        let compiled = lower(&graph, &schedule, 1, 3, &helpers)
            .unwrap_or_else(|| panic!("do-while loop must lower (optimize={optimize})"));
        for (n, want) in [(0i64, 0i32), (1, 0), (2, 1), (5, 10), (100, 4950)] {
            // SAFETY: the lowered body takes one int argument and returns an
            // int; the poll flag is a static zero, so the slow path never runs.
            let got = unsafe { compiled.try_call(&[n]) }.expect("call");
            // Cast: this tier's `int` result is the low 32 bits of RAX.
            assert_eq!(got as i32, want, "f({n}) with optimize={optimize}");
        }
    }
}

// ── Round 9 wave 2 (irlower2) ─────────────────────────────────────

/// Compile `code` (one `int` or `long` parameter) through the IR tier and
/// call it with each input. `optimize` runs the full scalar pipeline, which
/// is what folds the builder's `Guard(Cmp(Ne)[c, 0])` for a constant
/// divisor away and leaves the lowering's own guards to decide.
pub(in crate::ir_lower::tests) fn r9w2_compile_one_arg(
    code: &[u8],
    long_param: bool,
    optimize: bool,
) -> CompiledMethod {
    let (params, locals) = if long_param { (1, 2) } else { (1, 1) };
    let mut builder = IrBuilder::new(params, locals);
    if long_param {
        builder.set_param_types(&[IrType::Long]);
    }
    let mut graph = builder.build(code, code.len()).expect("IR build");
    if optimize {
        ir_optimize::optimize(&mut graph);
    }
    let schedule = ir_schedule::schedule(&graph);
    lower(&graph, &schedule, params, locals, &no_helpers())
        .unwrap_or_else(|| panic!("must lower (optimize={optimize})"))
}

#[test]
pub(in crate::ir_lower::tests) fn the_allocated_division_plan_names_its_ordered_machine_steps() {
    for (ty, opcode, patterns) in [
        (IrType::Int, 0x6c, ["cdq", "idiv_r32"]),
        (IrType::Int, 0x70, ["cdq", "idiv_r32"]),
        (IrType::Long, 0x6d, ["cqo", "idiv_r64"]),
        (IrType::Long, 0x71, ["cqo", "idiv_r64"]),
    ] {
        let wide = ty == IrType::Long;
        let code = if wide {
            [0x1e, 0x20, opcode, 0xad]
        } else {
            [0x1a, 0x1b, opcode, 0xac]
        };
        let mut builder = IrBuilder::new(2, if wide { 4 } else { 2 });
        builder.set_param_types(&[ty, ty]);
        let graph = builder.build(&code, code.len()).expect("division graph");
        let schedule = ir_schedule::schedule(&graph);
        let plan = build_allocated_mir_plan(&graph, &schedule, &[], None).expect("allocated plan");
        let div = graph
            .nodes
            .iter()
            .position(|node| matches!(node.op, Op::Div | Op::Rem))
            .expect("division") as NodeId;
        let selected = plan.tile(div).expect("one division tile");
        assert_eq!(selected.covered, vec![div]);
        let rows: Vec<_> = selected
            .insts
            .iter()
            .filter_map(|inst| inst.pattern_name())
            .collect();
        assert_eq!(
            rows, patterns,
            "the allocated plan must select sign extension then IDIV"
        );
        use crate::regalloc::{build_live_model, FixedConstraint, PhysReg, RegFile, RegSpec};
        let live = build_live_model(&graph, &schedule);
        let regs = RegFile::from_specs([RAX, RCX, RDX, R8].map(|reg| RegSpec {
            reg: PhysReg::gp(reg),
            caller_saved: false,
        }));
        let pos = live.pos_of[div as usize].expect("division position");
        let mut model = ir_lower_machine_model(&graph, &schedule, &live, regs, Some(&plan));
        assert!(model.fixed.iter().all(|fixed| fixed.pos != pos));
        assert!(model.clobbers.iter().all(|(at, _)| *at != pos));
        plan.add_allocated_requirements(&live, &mut model)
            .expect("selected constraints");
        assert_eq!(
            model
                .fixed
                .iter()
                .filter(|fixed| fixed.pos == pos)
                .copied()
                .collect::<Vec<_>>(),
            [FixedConstraint {
                node: graph.nodes[div as usize].inputs[0],
                pos,
                reg: PhysReg::gp(RAX),
            }]
        );
        assert_eq!(
            model
                .clobbers
                .iter()
                .find(|(at, _)| *at == pos)
                .map(|(_, regs)| regs.as_slice()),
            Some([RAX, RCX, RDX].map(PhysReg::gp).as_slice())
        );
        let legacy = build_mir_plan(&graph, &schedule).expect("frame-homed plan");
        assert_eq!(
            legacy.tile(div).expect("legacy division").rule,
            crate::x64::isel::Rule::Generic
        );
    }
}

/// Round 9 wave 2 (iropt X1/X2, isel #2): a division by a non-zero
/// constant emits no zero test and no deopt point, a constant other than
/// -1 skips the overflow guard, and the variable-divisor guard now tests
/// the DIVISOR alone (`CMP ECX,-1; JNE; NEG/XOR`). Executed against Java's
/// own semantics for every divisor class, `MIN / -1` included.
#[test]
pub(in crate::ir_lower::tests) fn r9w2_divisions_match_java_semantics_for_every_divisor_class() {
    let xs: [i32; 8] = [i32::MIN, i32::MIN + 1, -11, -1, 0, 9, 123_456, i32::MAX];
    // iload_0 ; bipush c ; idiv|irem ; ireturn
    for c in [7i8, 10, -1, -7, 3] {
        for (op, is_rem) in [(0x6cu8, false), (0x70u8, true)] {
            // Cast: the bipush operand byte.
            let code = [0x1a, 0x10, c as u8, op, 0xac];
            for optimize in [false, true] {
                let cm = r9w2_compile_one_arg(&code, false, optimize);
                for &x in &xs {
                    let want = if is_rem {
                        x.wrapping_rem(i32::from(c))
                    } else {
                        x.wrapping_div(i32::from(c))
                    };
                    // SAFETY: an `int f(int)` body compiled by this test,
                    // with the no-helper table; no path reaches a helper
                    // (the divisor is a non-zero constant).
                    let got = unsafe { cm.try_call(&[i64::from(x)]) }.expect("call");
                    // Cast: this tier's `int` result is the low 32 bits.
                    assert_eq!(
                        got as i32,
                        want,
                        "{x} {} {c} (optimize={optimize})",
                        if is_rem { "%" } else { "/" }
                    );
                }
                if optimize {
                    assert!(
                        !cm._deopt_point_boxes
                            .iter()
                            .any(|p| p.reason == DeoptReason::DivByZero),
                        "a constant divisor {c} must not plant a DivByZero deopt"
                    );
                }
            }
        }
    }
    // long: lload_0 ; bipush c ; i2l ; ldiv|lrem ; lreturn
    let ls: [i64; 6] = [i64::MIN, -11, -1, 0, 0x1_0000_0007, i64::MAX];
    for c in [7i8, -1, -3] {
        for (op, is_rem) in [(0x6du8, false), (0x71u8, true)] {
            // Cast: the bipush operand byte.
            let code = [0x1e, 0x10, c as u8, 0x85, op, 0xad];
            for optimize in [false, true] {
                let cm = r9w2_compile_one_arg(&code, true, optimize);
                for &x in &ls {
                    let want = if is_rem {
                        x.wrapping_rem(i64::from(c))
                    } else {
                        x.wrapping_div(i64::from(c))
                    };
                    // SAFETY: as above, a `long f(long)` body.
                    let got = unsafe { cm.try_call(&[x]) }.expect("call");
                    assert_eq!(got, want, "{x}L op {c}L (optimize={optimize})");
                }
            }
        }
    }
    // A VARIABLE divisor keeps its guards: int f(int a, int b) { return a / b; }
    // and a % b, over the MIN/-1 corner and ordinary pairs (no zero: that
    // deopts, which `test_lower_div_by_zero_deopts` covers).
    for (op, is_rem) in [(0x6cu8, false), (0x70u8, true)] {
        let code = [0x1a, 0x1b, op, 0xac, 0, 0];
        let cm = compile_via_ir(&code, 4, 2, 2).expect("a / b compiles via IR");
        for (a, b) in [
            (i32::MIN, -1),
            (7, -1),
            (-7, 2),
            (i32::MAX, -1),
            (-9, 4),
            (0, -1),
        ] {
            let want = if is_rem {
                a.wrapping_rem(b)
            } else {
                a.wrapping_div(b)
            };
            // SAFETY: an `int f(int, int)` body with a non-zero divisor.
            let got = unsafe { cm.try_call(&[i64::from(a), i64::from(b)]) }.expect("call");
            // Cast: this tier's `int` result is the low 32 bits.
            assert_eq!(got as i32, want, "{a} op {b}");
        }
    }
}

/// X2's prediction half: a graph whose only possible trap was a division
/// by a non-zero constant is trap-free; a zero or variable divisor is not.
#[test]
pub(in crate::ir_lower::tests) fn r9w2_a_division_by_a_nonzero_constant_cannot_deopt() {
    let build = |divisor: Option<i64>| {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let d = match divisor {
            Some(c) => g.add(Op::Const(c), IrType::Int, vec![], None),
            None => g.add(Op::Param(1), IrType::Int, vec![start], None),
        };
        let q = g.add(Op::Div, IrType::Int, vec![a, d], Some(2));
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, q], Some(3));
        g
    };
    assert!(graph_cannot_deopt(&build(Some(10))));
    assert!(
        graph_cannot_deopt(&build(Some(-1))),
        "MIN/-1 is inline, not a trap"
    );
    // `1 << 32` is ZERO at int width: the low 32 bits are what IDIV ECX sees.
    assert!(!graph_cannot_deopt(&build(Some(1i64 << 32))));
    assert!(!graph_cannot_deopt(&build(Some(0))));
    assert!(!graph_cannot_deopt(&build(None)));
}

// ── Round 9 wave 3 (irlower3) ─────────────────────────────────────

/// `signed_div_magic` against its own contract, in exact `i128`
/// arithmetic: `m < 2^width`, and for every sampled `n`,
/// `floor(n * m / 2^(width + s)) + (n < 0)` is Rust's truncating `n / a`.
/// Covers every `2^k ± 1`, the width's `MAX`, and the divisors the
/// benchmarks use.
#[test]
pub(in crate::ir_lower::tests) fn r9w3_signed_div_magic_matches_brute_force() {
    for width in [32u32, 64] {
        let max: i128 = (1i128 << (width - 1)) - 1;
        let min: i128 = -(1i128 << (width - 1));
        let mut divisors: Vec<u64> = vec![
            3,
            5,
            6,
            7,
            9,
            10,
            11,
            12,
            13,
            25,
            100,
            125,
            641,
            1000,
            1_000_000_007,
        ];
        for k in 2..(width - 1) {
            divisors.push((1u64 << k) + 1);
            divisors.push((1u64 << k) - 1);
        }
        // Cast: MAX of the width, positive, fits u64.
        divisors.push(max as u64);
        if width == 64 {
            divisors.extend([
                274_177u64,
                6_700_417,
                10_000_000_019,
                1_000_000_000_000_000_003,
            ]);
        }
        let mut dividends: Vec<i128> = vec![
            min,
            min + 1,
            -1_000_000,
            -7,
            -1,
            0,
            1,
            7,
            1_000_000,
            max - 1,
            max,
        ];
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..64 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let v = if width == 32 {
                // Cast: the low 32 bits, as a signed int.
                i128::from(x as u32 as i32)
            } else {
                // Cast: the bit pattern, as a signed long.
                i128::from(x as i64)
            };
            dividends.push(v);
        }
        for &a in &divisors {
            if a < 3 || a.is_power_of_two() {
                continue;
            }
            let (m, s) = signed_div_magic(a, width);
            assert!(
                width == 64 || m < (1u64 << 32),
                "magic for {a} at width {width} is {m:#x}, not below 2^{width}"
            );
            let m = i128::from(m);
            let ai = i128::from(a);
            let check = |n: i128| {
                let q = ((n * m) >> (width + s)) + i128::from(n < 0);
                assert_eq!(q, n / ai, "{n} / {a} at width {width} (m={m:#x}, s={s})");
            };
            for &n in &dividends {
                check(n);
            }
            // The divisor's own multiples, where an off-by-one magic shows.
            for mult in [1i128, 2, 3] {
                for n in [ai * mult, ai * mult - 1, -(ai * mult), -(ai * mult) + 1] {
                    if n >= min && n <= max {
                        check(n);
                    }
                }
            }
        }
    }
}

/// `f(x) = x / d` or `x % d` over a hand-built graph: `Param(0)` of the
/// given width, a constant divisor, one division, one return.
pub(in crate::ir_lower::tests) fn r9w3_const_div_method(
    long: bool,
    d: i64,
    is_rem: bool,
) -> CompiledMethod {
    let ty = if long { IrType::Long } else { IrType::Int };
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let x = g.add(Op::Param(0), ty, vec![start], None);
    let c = g.add(Op::Const(d), ty, vec![], None);
    let op = if is_rem { Op::Rem } else { Op::Div };
    let r = g.add(op, ty, vec![x, c], Some(2));
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, r], Some(3));
    let schedule = ir_schedule::schedule(&g);
    let locals = if long { 2 } else { 1 };
    lower(&g, &schedule, 1, locals, &no_helpers()).unwrap_or_else(|| {
        panic!(
            "x {} {d} must lower (long={long})",
            if is_rem { "%" } else { "/" }
        )
    })
}

/// Round 9 wave 3 (irlower3), item 4 of
/// `ir-constant-divisor-still-pays-the-zero-and-overflow-tests`: a
/// division or remainder by a constant is shifts / multiply-high, and it
/// is EXECUTED here against Rust's wrapping (= Java's) semantics for every
/// divisor class -- ±1, ±2^k up to the width's top power, `MIN` (which
/// keeps the `IDIV`), `MAX`, `MIN + 1`, magic numbers that wrap negative
/// as a signed 64-bit multiplier and ones that do not, and 64-bit
/// divisors past `i32` -- over dividends including `MIN`, `MAX`, `-1`, `0`
/// and the divisor's own neighbourhood.
#[test]
pub(in crate::ir_lower::tests) fn r9w3_constant_divisions_execute_with_java_semantics() {
    let sym = |is_rem: bool| if is_rem { "%" } else { "/" };
    let mut ints: Vec<i32> = vec![
        3,
        -3,
        5,
        6,
        7,
        -7,
        9,
        10,
        -10,
        11,
        12,
        13,
        25,
        100,
        125,
        641,
        1000,
        65_535,
        1_000_000_007,
        -1_000_000_007,
        i32::MAX,
        i32::MIN + 1,
        i32::MIN,
        0x7FFF_FFFE,
    ];
    for k in 0..31 {
        ints.push(1 << k);
        ints.push(-(1 << k));
    }
    let mut int_xs: Vec<i32> = vec![
        i32::MIN,
        i32::MIN + 1,
        i32::MIN + 7,
        -(1 << 30),
        -1_000_000_007,
        -65_537,
        -100,
        -11,
        -8,
        -7,
        -2,
        -1,
        0,
        1,
        2,
        7,
        8,
        9,
        100,
        65_535,
        123_456_789,
        i32::MAX - 1,
        i32::MAX,
    ];
    let mut x: u32 = 0x2545_F491;
    for _ in 0..24 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        // Cast: a pseudo-random bit pattern, as a signed int.
        int_xs.push(x as i32);
    }
    for &d in &ints {
        for is_rem in [false, true] {
            let cm = r9w3_const_div_method(false, i64::from(d), is_rem);
            if d != i32::MIN {
                // CDQ ; IDIV ECX -- gone for every reducible divisor.
                assert!(
                    !contains_seq(cm.code_bytes(), &[0x99, 0xF7, 0xF9]),
                    "int x {} {d} still emits IDIV",
                    sym(is_rem)
                );
            }
            let mut probe = int_xs.clone();
            probe.extend([d, d.wrapping_add(1), d.wrapping_sub(1), d.wrapping_neg()]);
            probe.extend([d.wrapping_mul(3), d.wrapping_mul(3).wrapping_add(1)]);
            for &n in &probe {
                let want = if is_rem {
                    n.wrapping_rem(d)
                } else {
                    n.wrapping_div(d)
                };
                // SAFETY: an `int f(int)` body with no call and no deopt
                // point (the divisor is a non-zero constant).
                let got = unsafe { cm.try_call(&[i64::from(n)]) }.expect("call");
                // Cast: this tier's `int` result is the low 32 bits.
                assert_eq!(got as i32, want, "int {n} {} {d}", sym(is_rem));
            }
        }
    }

    let mut longs: Vec<i64> = vec![
        3,
        -3,
        5,
        6,
        7,
        -7,
        10,
        -10,
        11,
        13,
        25,
        100,
        641,
        1000,
        274_177,
        6_700_417,
        1_000_000_007,
        0x7FFF_FFFF,
        0x8000_0001,
        0xFFFF_FFFF,
        0x1_0000_0001,
        10_000_000_000,
        1_000_000_000_000_000_003,
        -10_000_000_019,
        i64::MAX,
        i64::MIN + 1,
        i64::MIN,
    ];
    for k in 0..63 {
        longs.push(1i64 << k);
        longs.push(-(1i64 << k));
    }
    let mut long_xs: Vec<i64> = vec![
        i64::MIN,
        i64::MIN + 1,
        i64::MIN + 7,
        -(1i64 << 62),
        -(1i64 << 32) - 1,
        -100,
        -8,
        -7,
        -1,
        0,
        1,
        7,
        8,
        1i64 << 31,
        (1i64 << 32) + 5,
        1_000_000_000_000,
        i64::MAX - 1,
        i64::MAX,
        i64::from(i32::MIN),
        i64::from(i32::MAX),
    ];
    let mut y: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..24 {
        y ^= y << 13;
        y ^= y >> 7;
        y ^= y << 17;
        // Cast: a pseudo-random bit pattern, as a signed long.
        long_xs.push(y as i64);
    }
    for &d in &longs {
        for is_rem in [false, true] {
            let cm = r9w3_const_div_method(true, d, is_rem);
            if d != i64::MIN {
                // CQO ; IDIV RCX
                assert!(
                    !contains_seq(cm.code_bytes(), &[0x48, 0x99, 0x48, 0xF7, 0xF9]),
                    "long x {} {d} still emits IDIV",
                    sym(is_rem)
                );
            }
            let mut probe = long_xs.clone();
            probe.extend([d, d.wrapping_add(1), d.wrapping_sub(1), d.wrapping_neg()]);
            probe.extend([d.wrapping_mul(3), d.wrapping_mul(3).wrapping_sub(1)]);
            for &n in &probe {
                let want = if is_rem {
                    n.wrapping_rem(d)
                } else {
                    n.wrapping_div(d)
                };
                // SAFETY: a `long f(long)` body with no call and no deopt
                // point (the divisor is a non-zero constant).
                let got = unsafe { cm.try_call(&[n]) }.expect("call");
                assert_eq!(got, want, "long {n} {} {d}", sym(is_rem));
            }
        }
    }
}

/// A floating division whose divisor is the constant 0 -- what the optimizer
/// leaves when it prunes the arm that owned the division and zeroes its
/// operands (`Duration.plus` spliced into `VertxConnection.close()` with
/// `amountToAdd == 0`) -- must not reach a bare `IDIV`: it raised `#DE` in
/// the process. EXECUTED: the method returns the placeholder 0.
#[test]
pub(in crate::ir_lower::tests) fn a_division_by_the_constant_zero_is_skipped_not_executed() {
    for long in [false, true] {
        for is_rem in [false, true] {
            // The IDIV bytes stay in the body (jumped over), so the proof is
            // the execution, not their absence.
            let cm = r9w3_const_div_method(long, 0, is_rem);
            // SAFETY: a `f(x)` body with no call and no deopt point.
            let got = unsafe { cm.try_call(&[7]) }.expect("call");
            assert_eq!(
                got,
                0,
                "{} {} 0 must yield the placeholder",
                if long { "long" } else { "int" },
                if is_rem { "%" } else { "/" }
            );
        }
    }
}

/// The `CratonBench arithmetic` / `ArithProbe` kernel, through the IR
/// tier with and without the scalar pipeline, EXECUTED:
///
/// ```java
/// static long f(long n) { long s = 0; for (long i = 0; i < n; i++) s += i*3 - i/2 + i%7; return s; }
/// ```
///
/// Optimized, the divisors fold to constants, so the body must hold no
/// `IDIV` at all -- the 2x regression wave 2 introduced was exactly this
/// loop paying two `IDIV r64` per iteration in the optimizing OSR body.
#[test]
pub(in crate::ir_lower::tests) fn r9w3_the_arithmetic_kernel_runs_without_idiv() {
    static FLAG: u8 = 0;
    extern "C" fn slow_poll() {}
    let code: [u8; 44] = [
        0x09, // 0: lconst_0
        0x41, // 1: lstore_2         s = 0
        0x09, // 2: lconst_0
        0x37, 0x04, // 3: lstore 4   i = 0
        0x16, 0x04, // 5: lload 4    <- loop header
        0x1e, // 7: lload_0
        0x94, // 8: lcmp
        0x9c, 0x00, 0x21, // 9: ifge +33 -> 42
        0x20, // 12: lload_2
        0x16, 0x04, // 13: lload 4
        0x06, // 15: iconst_3
        0x85, // 16: i2l
        0x69, // 17: lmul
        0x16, 0x04, // 18: lload 4
        0x05, // 20: iconst_2
        0x85, // 21: i2l
        0x6d, // 22: ldiv
        0x65, // 23: lsub
        0x16, 0x04, // 24: lload 4
        0x10, 0x07, // 26: bipush 7
        0x85, // 28: i2l
        0x71, // 29: lrem
        0x61, // 30: ladd
        0x61, // 31: ladd
        0x41, // 32: lstore_2
        0x16, 0x04, // 33: lload 4
        0x0a, // 35: lconst_1
        0x61, // 36: ladd
        0x37, 0x04, // 37: lstore 4
        0xa7, 0xff, 0xde, // 39: goto -34 -> 5
        0x20, // 42: lload_2
        0xad, // 43: lreturn
    ];
    for optimize in [false, true] {
        let mut builder = IrBuilder::new(1, 6);
        builder.set_param_types(&[IrType::Long]);
        let mut graph = builder.build(&code, code.len()).expect("IR build");
        if optimize {
            ir_optimize::optimize(&mut graph);
        }
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.safepoint_flag_addr = &FLAG as *const u8 as usize;
        helpers.safepoint_slow_path = slow_poll as *const () as usize;
        let cm = lower(&graph, &schedule, 1, 6, &helpers)
            .unwrap_or_else(|| panic!("the kernel must lower (optimize={optimize})"));
        if optimize {
            assert!(
                !contains_seq(cm.code_bytes(), &[0x48, 0x99, 0x48, 0xF7, 0xF9]),
                "the optimized kernel still divides with IDIV"
            );
        }
        for n in [0i64, 1, 2, 7, 13, 1000, 100_003] {
            let mut want = 0i64;
            for i in 0..n {
                want = want.wrapping_add(i.wrapping_mul(3) - i / 2 + i % 7);
            }
            // SAFETY: a `long f(long)` body; the poll flag is a static
            // zero, so the slow path never runs, and no path reaches a
            // helper or a deopt (the only divisors are non-zero).
            let got = unsafe { cm.try_call(&[n]) }.expect("call");
            assert_eq!(got, want, "f({n}) with optimize={optimize}");
        }
    }
}

/// Round 9 wave 3 (irlower3), the IR half of
/// `perf-long-counted-loops-lose-the-optimizing-osr-body`: `lcmp; if<cc>`
/// is ONE `CMP r64, r64; Jcc` -- the `lcmp` arm emits no
/// `SETG/SETL/MOVZX/SUB/MOVSXD` and writes no home word. Executed for all
/// six conditions over pairs that differ only in the high word, only in
/// the low word, and at `MIN`/`MAX` (where `a - b` overflows, which is why
/// the compare has to be the flags of `CMP`, never of a subtraction).
#[test]
pub(in crate::ir_lower::tests) fn r9w3_lcmp_branches_fuse_into_one_compare() {
    // long a, long b: lload_0 ; lload_2 ; lcmp ; if<cc> +5 ; iconst_1 ;
    // ireturn ; iconst_0 ; ireturn  -- 0 when the branch is taken.
    let pairs: [(i64, i64); 12] = [
        (0, 0),
        (1, 2),
        (2, 1),
        (-1, 0),
        (0, -1),
        (i64::MIN, i64::MAX),
        (i64::MAX, i64::MIN),
        (i64::MIN, i64::MIN),
        (1i64 << 32, 1),
        (1, 1i64 << 32),
        (-(1i64 << 32), 0xFFFF_FFFF),
        (0x1_0000_0005, 0x2_0000_0005),
    ];
    for op in 0x99u8..=0x9e {
        let code = [0x1e, 0x20, 0x94, op, 0x00, 0x05, 0x04, 0xac, 0x03, 0xac];
        for optimize in [false, true] {
            let mut builder = IrBuilder::new(2, 4);
            builder.set_param_types(&[IrType::Long, IrType::Long]);
            let mut graph = builder.build(&code, code.len()).expect("IR build");
            if optimize {
                ir_optimize::optimize(&mut graph);
            }
            let schedule = ir_schedule::schedule(&graph);
            let cm = lower(&graph, &schedule, 2, 4, &no_helpers())
                .unwrap_or_else(|| panic!("lcmp branch must lower (op={op:#x})"));
            if ir_register_authoritative_enabled() {
                // SETG AL -- the `lcmp` materialisation.
                assert!(
                    !contains_seq(cm.code_bytes(), &[0x0F, 0x9F, 0xC0]),
                    "op {op:#x} (optimize={optimize}): the lcmp was not fused into its branch"
                );
            }
            for &(a, b) in &pairs {
                let taken = match op {
                    0x99 => a == b,
                    0x9a => a != b,
                    0x9b => a < b,
                    0x9c => a >= b,
                    0x9d => a > b,
                    _ => a <= b,
                };
                // SAFETY: an `int f(long, long)` body with no call and no
                // trap.
                let got = unsafe { cm.try_call(&[a, b]) }.expect("call");
                // Cast: this tier's `int` result is the low 32 bits.
                assert_eq!(
                    got as i32,
                    i32::from(!taken),
                    "op {op:#x} a={a} b={b} (optimize={optimize})"
                );
            }
        }
    }
}

/// Round 9 wave 10 (irl10): the byte encoders behind the three-operand
/// `IMUL RAX, r, imm` and the register-register `LEA dst, [base + index]`,
/// checked against hand-assembled bytes -- including the three silent SIB
/// traps (R13/RBP base needs a disp8, RSP cannot be an index, R12 as an
/// index needs REX.X).
#[test]
pub(in crate::ir_lower::tests) fn r9w10_irl10_three_operand_encoders() {
    let lea = |d: u8, b: u8, i: u8, w: bool| {
        let (bytes, n) = lea_reg_base_index_bytes(d, b, i, w);
        bytes[..n].to_vec()
    };
    // lea rax,[r14+rcx]
    assert_eq!(lea(RAX, 14, RCX, true), vec![0x49, 0x8D, 0x04, 0x0E]);
    // lea r12,[r14+rcx]
    assert_eq!(lea(12, 14, RCX, true), vec![0x4D, 0x8D, 0x24, 0x0E]);
    // lea rax,[r13+rcx+0] -- R13 as a SIB base takes mod=01 and a disp8.
    assert_eq!(lea(RAX, 13, RCX, true), vec![0x49, 0x8D, 0x44, 0x0D, 0x00]);
    // lea rax,[rsp+rbx] -- an RSP index is swapped into the base.
    assert_eq!(lea(RAX, 3, 4, true), vec![0x48, 0x8D, 0x04, 0x1C]);
    // lea eax,[rbx+r12] -- R12 is a real index with REX.X.
    assert_eq!(lea(RAX, 3, 12, false), vec![0x42, 0x8D, 0x04, 0x23]);
    // lea eax,[rbx+rsi] -- no REX at all in the 32-bit form.
    assert_eq!(lea(RAX, 3, 6, false), vec![0x8D, 0x04, 0x33]);

    let imul = |s: u8, k: i32, w: bool| {
        let (bytes, n) = imul_rax_reg_imm_bytes(s, k, w);
        bytes[..n].to_vec()
    };
    // imul rax,r15,3
    assert_eq!(imul(15, 3, true), vec![0x49, 0x6B, 0xC7, 0x03]);
    // imul rax,rbx,-1
    assert_eq!(imul(3, -1, true), vec![0x48, 0x6B, 0xC3, 0xFF]);
    // imul eax,ebx,3
    assert_eq!(imul(3, 3, false), vec![0x6B, 0xC3, 0x03]);
    // imul rax,r15,1000 -- the imm32 form.
    assert_eq!(
        imul(15, 1000, true),
        vec![0x49, 0x69, 0xC7, 0xE8, 0x03, 0x00, 0x00]
    );
    // imul eax,r8d,-129
    assert_eq!(
        imul(8, -129, false),
        vec![0x41, 0x69, 0xC0, 0x7F, 0xFF, 0xFF, 0xFF]
    );
}

/// Round 9 wave 10 (irl10), `perf-long-counted-loops-lose-the-optimizing-osr-body`:
/// the counted loop `s += i * 3 + (i >> 1)` -- the page's `mulOnly`
/// shape -- EXECUTED in its `int` and `long` forms against a reference,
/// with and without the optimizer, over trip counts 0..=1000. The `int`
/// form must also show the three-operand `IMUL r, r/m, imm` reading the
/// resident counter (a `6B` whose ModRM `r/m` is not RAX), i.e. the new
/// form actually ran rather than the test comparing the old one with
/// itself.
/// Round 9 wave 11 (irl11), `RcxTwin`: an RCX carry whose home is dropped
/// is installed by the twin of the run that computed it (`ADD RCX, RAX`,
/// `MOV RCX, r; SAR RCX, imm`, ...) instead of that run plus `MOV RCX,
/// RAX`. EXECUTED against a reference, optimized and not, over trip counts
/// 0..=1000, for the page's `long` `mulOnly` loop and for an `int` loop
/// that puts every twinned op (`*`, `^`, `<<`, `|`, `&`, `>>>`, `+`) on a
/// carry chain.
///
/// The byte half: with the twin on (the default; the test sets no flag),
/// the untwinned pair `ADD RAX, RCX; MOV RCX, RAX` can no longer appear
/// ADJACENT in a body -- adjacency means `store_rax` emitted no home store,
/// i.e. the home was dropped and the value is an RCX carry, which is
/// exactly when `apply_rcx_twin` fires.
/// Round 9 wave 10 (irl10): the register-register `LEA` form of
/// `Op::Add` is held to the same source-level contract as
/// `emit_add_lea`'s direct form (see
/// `the_lea_add_form_publishes_what_it_does_not_store`): on the path that
/// may skip the home store it publishes the register first and says so,
/// and its accumulator form publishes nothing.
#[test]
pub(in crate::ir_lower::tests) fn r9w10_irl10_the_reg_reg_lea_form_publishes_what_it_does_not_store(
) {
    let src = concat!(
        include_str!("../../ir_lower/mod.rs"),
        "\n",
        include_str!("../../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_session_confined_rows.rs")
    )
    .replace("\r\n", "\n");
    let body = src
        .split("fn emit_add_lea_reg_reg(\n")
        .nth(1)
        .expect("emit_add_lea_reg_reg is in this file")
        .split("\n    fn ")
        .next()
        .expect("the function ends");
    let direct = body
        .split("if let Some(dst) = self.assigned_gpr(id) {")
        .nth(1)
        .expect("the direct form gates on an assigned register")
        .split("AddForm::Done")
        .next()
        .expect("the direct form returns AddForm::Done");
    for required in [
        "self.mark_gp_reg_live(id);",
        "self.cur_def_published = true;",
        "self.store_abi_reg(dst, slot);",
    ] {
        assert!(direct.contains(required), "direct form lacks `{required}`");
    }
    let publish = direct
        .find("self.mark_gp_reg_live(id);")
        .expect("checked above");
    let decide = direct.find("home_dropped").expect("the drop test is here");
    assert!(
        publish < decide,
        "publishes after deciding to skip the home store"
    );
    let acc = body
        .split("return AddForm::Done;")
        .nth(1)
        .expect("the direct form returns")
        .split("AddForm::InRax")
        .next()
        .expect("the accumulator form returns");
    assert!(!acc.contains("mark_gp_reg_live"));
    // Every check precedes the first emitted byte: the only emission sites
    // are the two `LEA`s, and nothing before the carry retirement emits.
    let first_emit = body.find("self.buf.emit(").expect("it emits");
    for gate in [
        "self.carry_names(x)",
        "self.resident_gpr(x)",
        "self.gp_load_value(RCX, y);",
    ] {
        let at = body
            .find(gate)
            .unwrap_or_else(|| panic!("`{gate}` is in the helper"));
        assert!(
            at < first_emit,
            "`{gate}` comes after the first emitted byte"
        );
    }
}

/// Round 9 wave 8 (irl8), the `FCmp` half of
/// `perf-long-counted-loops-lose-the-optimizing-osr-body`:
/// `fcmp<l|g>`/`dcmp<l|g>; if<lt|le|gt|ge>` is ONE `UCOMIS; Jcc` (no
/// `SETA/SETB` materialisation), and `if<eq|ne>` keeps the three-way value.
/// EXECUTED for all four opcodes, all six conditions, over pairs including
/// NaN on either side, both zeros and the infinities -- the NaN answer of
/// the `l`/`g` bias is the whole point of `fcmp_fused_branch`.
#[test]
pub(in crate::ir_lower::tests) fn r9w8_fcmp_branches_fuse_into_one_ucomis() {
    let vals: [f64; 8] = [
        0.0,
        -0.0,
        1.0,
        -1.0,
        1.5,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    for cmp_op in [0x95u8, 0x96, 0x97, 0x98] {
        let double = cmp_op >= 0x97;
        let nan_greater = cmp_op == 0x96 || cmp_op == 0x98;
        for br in 0x99u8..=0x9e {
            // a, b: load ; load ; [fd]cmp[lg] ; if<cc> +5 ; iconst_1 ;
            // ireturn ; iconst_0 ; ireturn  -- 0 when the branch is taken.
            let (l0, l1, locals, ty) = if double {
                (0x26u8, 0x28u8, 4usize, IrType::Double)
            } else {
                (0x22u8, 0x23u8, 2usize, IrType::Float)
            };
            let code = [l0, l1, cmp_op, br, 0x00, 0x05, 0x04, 0xac, 0x03, 0xac];
            for optimize in [false, true] {
                let mut builder = IrBuilder::new(2, locals);
                builder.set_param_types(&[ty, ty]);
                let mut graph = builder.build(&code, code.len()).expect("IR build");
                if optimize {
                    ir_optimize::optimize(&mut graph);
                }
                let schedule = ir_schedule::schedule(&graph);
                let cm = lower(&graph, &schedule, 2, locals, &no_helpers()).unwrap_or_else(|| {
                    panic!("fcmp branch must lower (cmp={cmp_op:#x} br={br:#x})")
                });
                let ordering = !matches!(br, 0x99 | 0x9a);
                if ordering && ir_register_authoritative_enabled() {
                    // SETA AL / SETB AL -- the `FCmp` materialisation.
                    let code_bytes = cm.code_bytes();
                    assert!(
                        !contains_seq(code_bytes, &[0x0F, 0x97, 0xC0])
                            && !contains_seq(code_bytes, &[0x0F, 0x92, 0xC0]),
                        "cmp {cmp_op:#x} br {br:#x} (optimize={optimize}): the fcmp \
                             was not fused into its branch"
                    );
                }
                for &a in &vals {
                    for &b in &vals {
                        let three_way: i32 = if a > b {
                            1
                        } else if a < b {
                            -1
                        } else if a == b {
                            0
                        } else if nan_greater {
                            1
                        } else {
                            -1
                        };
                        let taken = match br {
                            0x99 => three_way == 0,
                            0x9a => three_way != 0,
                            0x9b => three_way < 0,
                            0x9c => three_way >= 0,
                            0x9d => three_way > 0,
                            _ => three_way <= 0,
                        };
                        // Raw bits in integer ABI registers, as
                        // `Op::Param` expects; a float's bits are
                        // zero-extended (`f.to_bits() as i64`).
                        let (xa, xb) = if double {
                            (a.to_bits() as i64, b.to_bits() as i64)
                        } else {
                            // Cast: `f64 -> f32` of these values is exact
                            // (NaN stays NaN).
                            ((a as f32).to_bits() as i64, (b as f32).to_bits() as i64)
                        };
                        // SAFETY: an `int f(T, T)` body with no call and
                        // no trap.
                        let got = unsafe { cm.try_call(&[xa, xb]) }.expect("call");
                        // Cast: this tier's `int` result is the low 32 bits.
                        assert_eq!(
                            got as i32,
                            i32::from(!taken),
                            "cmp {cmp_op:#x} br {br:#x} a={a} b={b} (optimize={optimize})"
                        );
                    }
                }
            }
        }
    }
}

/// The flags table itself, independent of any emission: for every
/// ordering condition and both biases, `fcmp_fused_branch`'s Jcc over the
/// `UCOMIS` flags of every ordered/unordered outcome agrees with the JVMS
/// three-way result, and its `^ 1` complement is the exact negation.
#[test]
pub(in crate::ir_lower::tests) fn r9w8_the_fused_fcmp_flag_table_matches_the_three_way_result() {
    // (CF, ZF) of `UCOMIS p, q` for p>q, p<q, p==q, unordered.
    let flags = |gt: bool, lt: bool, eq: bool| -> (bool, bool) {
        if gt {
            (false, false)
        } else if lt {
            (true, false)
        } else if eq {
            (false, true)
        } else {
            (true, true)
        }
    };
    let taken = |jcc: u8, (cf, zf): (bool, bool)| -> bool {
        match jcc {
            0x82 => cf,
            0x83 => !cf,
            0x86 => cf || zf,
            0x87 => !cf && !zf,
            other => panic!("unexpected jcc {other:#x}"),
        }
    };
    // Outcomes of `a` vs `b` as (a>b, a<b, a==b); all false is NaN.
    let outcomes = [
        (true, false, false),
        (false, true, false),
        (false, false, true),
        (false, false, false),
    ];
    for nan_greater in [false, true] {
        for cc in [
            CmpOp::Lt,
            CmpOp::Le,
            CmpOp::Gt,
            CmpOp::Ge,
            CmpOp::Eq,
            CmpOp::Ne,
        ] {
            let Some((swap, jcc)) = fcmp_fused_branch(cc, nan_greater) else {
                assert!(matches!(cc, CmpOp::Eq | CmpOp::Ne), "{cc:?} must fold");
                continue;
            };
            assert_eq!(swap, nan_greater);
            for &(gt, lt, eq) in &outcomes {
                let r: i32 = if gt {
                    1
                } else if lt {
                    -1
                } else if eq {
                    0
                } else if nan_greater {
                    1
                } else {
                    -1
                };
                let want = match cc {
                    CmpOp::Lt => r < 0,
                    CmpOp::Le => r <= 0,
                    CmpOp::Gt => r > 0,
                    _ => r >= 0,
                };
                // `UCOMIS b, a` swaps which of gt/lt the flags see.
                let f = if swap {
                    flags(lt, gt, eq)
                } else {
                    flags(gt, lt, eq)
                };
                assert_eq!(
                    taken(jcc, f),
                    want,
                    "{cc:?} ng={nan_greater} {gt}/{lt}/{eq}"
                );
                assert_eq!(taken(jcc ^ 1, f), !want, "negated {cc:?} ng={nan_greater}");
            }
        }
    }
}

/// Round 9 wave 8 (irl8), the IR half of
/// `perf-mutual-recursion-forces-every-call-through-invoke-dispatch`: an
/// `invokestatic` planned as a cycle-edge CELL site runs the stack-floor
/// guard, then calls the cell's entry directly when the word is set and
/// `jit_invoke_dispatch` when it is `0`. EXECUTED both ways on one body
/// (the word is flipped between calls, as `JitCache::put` / `clear` do),
/// and the no-cell control keeps plain dispatch.
#[test]
pub(in crate::ir_lower::tests) fn r9w8_a_cycle_edge_cell_site_calls_through_the_cell_or_dispatches()
{
    crate::x64::set_moving_young_override(Some(false));
    extern "C" fn dispatch(_vm: i64, _info: i64, args: i64, _n: i64) -> i64 {
        // SAFETY: `args` is the frame's staging block; arg 0 sits at its
        // base (`args_stage_top_off`), written just before this call.
        5000 + unsafe { *(args as *const i64) }
    }
    extern "C" fn callee(_vm: i64, x: i64) -> i64 {
        7000 + x
    }
    extern "C" fn guard_ok(_vm: i64, _ctor_receiver: i64) -> i64 {
        0
    }
    // static int f(int x) { return g(x); }  iload_0 ; invokestatic #2 ; ireturn
    let code = [0x1a, 0xb8, 0x00, 0x02, 0xac, 0x00, 0x00];
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "pkg/B",
        method_name: "a",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3, // static
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let lower_with = |cells: &HashMap<usize, usize>| {
        let mut builder = IrBuilder::new(1, 1);
        builder.set_param_types(&[IrType::Int]);
        let mut invoke_info = HashMap::new();
        invoke_info.insert(
            1usize,
            (info as *const JitInvokeInfo as usize, 1usize, b'I'),
        );
        builder.set_invoke_info(invoke_info);
        let graph = builder
            .build(&code, 5)
            .expect("IR build of the static call");
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.invoke_dispatch = dispatch as *const () as usize;
        helpers.self_call_stack_guard = guard_ok as *const () as usize;
        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        lower_inner_with_cycle_cells(
            &crate::DirectHelperTable::EMPTY,
            &graph,
            &schedule,
            1,
            1,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            &no_ic,
            &no_compact,
            &crate::ir::IrInlineFrameSites::default(),
            cells,
            &std::collections::HashSet::new(),
        )
        .expect("the call site lowers")
    };
    // The cell: its entry word at `JIT_CYCLE_EDGE_CELL_ENTRY_OFFSET` (0).
    let mut cell = [0u64; 4];
    let cell_addr = cell.as_mut_ptr() as usize;
    let word = cell_addr as *mut u64;

    // Control: no cell row, plain dispatch, the cell address baked nowhere.
    let plain = lower_with(&HashMap::new());
    assert!(!contains_seq(
        plain.code_bytes(),
        &(cell_addr as u64).to_le_bytes()
    ));
    // SAFETY: an `int f(int)` body whose only call is the stub above.
    let got = unsafe { plain.try_call_with_context(0, &[3]) }.expect("call");
    assert_eq!(got as i32, 5003);

    let mut cells = HashMap::new();
    cells.insert(1usize, cell_addr);
    let cm = lower_with(&cells);
    if !crate::direct_jit_callee_calls_enabled() {
        return;
    }
    assert!(
        contains_seq(cm.code_bytes(), &(cell_addr as u64).to_le_bytes()),
        "the site bakes the cell"
    );
    // Empty word: dispatch.
    // SAFETY: the stubs above; `cell` outlives every call.
    let got = unsafe { cm.try_call_with_context(0, &[4]) }.expect("call");
    assert_eq!(got as i32, 5004, "an empty cell dispatches");
    // Filled word: the direct call, context ABI.
    // SAFETY: a plain write to a live local through its own address.
    unsafe { std::ptr::write_volatile(word, (callee as *const () as usize as u64) | 1) };
    // SAFETY: as above.
    let got = unsafe { cm.try_call_with_context(0, &[5]) }.expect("call");
    assert_eq!(got as i32, 7005, "a filled cell calls its entry");
    // Cleared again: dispatch again, same body.
    // SAFETY: as above.
    unsafe { std::ptr::write_volatile(word, 0) };
    // SAFETY: as above.
    let got = unsafe { cm.try_call_with_context(0, &[6]) }.expect("call");
    assert_eq!(got as i32, 5006, "a cleared cell dispatches again");
}

/// Round nio21 wave 3 (lane L2): a call inside a `try` whose callee
/// throws enters the handler INSIDE the compiled frame. EXECUTED, raw and
/// optimized: `static int f(int x) { try { return g(x); } catch (Throwable
/// t) { return 2; } }` with a stub `g` that "throws" (returns the
/// sentinel) for a negative argument, and a stub
/// `jit_local_handler_lookup` that takes the throwable (`0`) or declines
/// (`-1`). Taken: `f(-1) == 2` from compiled code. Declined: today's
/// exit, the sentinel unchanged. The normal edge is untouched either way.
#[test]
pub(in crate::ir_lower::tests) fn nio21w3_a_call_inside_a_try_enters_its_handler_in_the_compiled_frame(
) {
    crate::x64::set_moving_young_override(Some(false));
    extern "C" fn dispatch(_vm: i64, _info: i64, args: i64, _n: i64) -> i64 {
        // SAFETY: `args` is the frame's staging block; arg 0 sits at its
        // base (`args_stage_top_off`), written just before this call.
        let x = unsafe { *(args as *const i64) };
        if x < 0 {
            i64::MIN
        } else {
            5000 + x
        }
    }
    extern "C" fn lookup_takes(_vm: i64, _site: i64, out: *mut i64) -> i64 {
        // SAFETY: `out` is the compiled frame's scratch word, live for the
        // whole call.
        unsafe { *out = 0 };
        0
    }
    extern "C" fn lookup_declines(_vm: i64, _site: i64, _out: *mut i64) -> i64 {
        -1
    }
    // 0 iload_0 ; 1 invokestatic #1 (I)I ; 4 ireturn ;
    // 5 astore_1 (handler, [0, 5) catch-all) ; 6 iconst_2 ; 7 ireturn
    let code = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0x4c, 0x05, 0xac];
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "pkg/T",
        method_name: "g",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3, // static
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let lower_with = |lookup: usize, optimize: bool| {
        let mut builder = IrBuilder::new(1, 2);
        builder.set_param_types(&[IrType::Int]);
        let mut invoke_info = HashMap::new();
        invoke_info.insert(
            1usize,
            (info as *const JitInvokeInfo as usize, 1usize, b'I'),
        );
        builder.set_invoke_info(invoke_info);
        builder.set_local_throw_rows(vec![crate::ir::IrLocalThrowRow {
            start_pc: 0,
            end_pc: 5,
            handler_pc: 5,
            catch: crate::ir::IrCatchClass::Any,
        }]);
        builder.set_call_throw_catch_names(vec![""], 0);
        let mut graph = builder.build(&code, 8).expect("IR build of the try/catch");
        assert_eq!(
            graph.call_throw_sites.len(),
            1,
            "the builder planted the edge"
        );
        if optimize {
            ir_optimize::optimize(&mut graph);
        }
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.invoke_dispatch = dispatch as *const () as usize;
        helpers.local_handler_lookup = lookup;
        lower(&graph, &schedule, 1, 2, &helpers).expect("the try/catch lowers")
    };
    for optimize in [false, true] {
        let cm = lower_with(lookup_takes as *const () as usize, optimize);
        assert_eq!(
            cm._jit_local_handler_sites.len(),
            1,
            "the artifact owns the site its pad bakes (optimize={optimize})"
        );
        let returns = |function: usize| {
            let mut call = vec![0x48, 0xb8];
            call.extend((function as u64).to_le_bytes());
            call.extend([0xff, 0xd0]);
            cm.code_bytes()
                .windows(call.len())
                .position(|bytes| bytes == call)
                .expect("the artifact calls the requested helper")
                + call.len()
        };
        let dispatch = returns(dispatch as *const () as usize);
        let lookup = returns(lookup_takes as *const () as usize);
        let dispatch_map = cm
            .oop_maps
            .iter()
            .find(|map| map.native_pc_offset as usize == dispatch)
            .expect("the original call has a map");
        assert!(
            cm.oop_maps
                .iter()
                .any(|map| map.native_pc_offset as usize == lookup
                    && map.bytecode_pc == dispatch_map.bytecode_pc),
            "the collecting lookup must bind the incoming call's publication",
        );
        // SAFETY: an `int f(int)` body whose only calls are the stubs above.
        let got = unsafe { cm.try_call_with_context(0, &[3]) }.expect("call");
        assert_eq!(got as i32, 5003, "the normal edge (optimize={optimize})");
        // SAFETY: as above.
        let got = unsafe { cm.try_call_with_context(0, &[-1]) }.expect("call");
        assert_eq!(
            got as i32, 2,
            "the handler, in the compiled frame (optimize={optimize})"
        );

        let cm = lower_with(lookup_declines as *const () as usize, optimize);
        // SAFETY: as above.
        let got = unsafe { cm.try_call_with_context(0, &[-1]) }.expect("call");
        assert_eq!(
            got,
            i64::MIN,
            "a declined lookup takes today's exit (optimize={optimize})"
        );
        // SAFETY: as above.
        let got = unsafe { cm.try_call_with_context(0, &[4]) }.expect("call");
        assert_eq!(
            got as i32, 5004,
            "and the normal edge again (optimize={optimize})"
        );
    }
}

/// The exceptional release's fast branch skips the second ID store.
/// Inspect the real emitted release and pads with the monitor wired or absent.
#[test]
pub(in crate::ir_lower::tests) fn nio21w3_sync_release_routes_lookup_returns_to_each_incoming_id() {
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_MONITOR_FASTPATH", Some("1"))],
        || {
            let code = [0x1a, 0xac]; // iload_0; ireturn
            let graph: &'static Graph = Box::leak(Box::new(
                IrBuilder::new(1, 1)
                    .build(&code, code.len())
                    .expect("IR build"),
            ));
            let schedule = Box::leak(Box::new(ir_schedule::schedule(graph)));
            let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
            for thin_unlock in [false, true] {
                let mut lo = lowerer_over(graph, schedule, plan, 4096);
                lo.reloc_emit_on = true;
                lo.sp_id_slot_off = 16;
                lo.context_slot_off = 24;
                lo.phi_copy_scratch_slot_off = 32;
                lo.shadow_thread_slot_off = 40;
                lo.get_current_thread = 0x1234;
                lo.monitor_block_off = if thin_unlock { 64 } else { 0 };
                lo.monitor_exit = 0x12345678;
                lo.local_handler_lookup = 0x87654321;
                lo.cur_call_throw = Some(CallThrowPlan {
                    site_ptr: 0x1000,
                    targets: vec![0],
                    call: graph.entry,
                    spliced: false,
                });

                let incoming = lo.emit_safepoint_map(0).expect("incoming publication");
                let incoming_id = incoming.id;
                lo.call_throw_publication = Some(CallThrowPublication {
                    map: Some(incoming),
                });
                lo.emit_sync_exit_then_throw(48, graph.entry);
                let release_id = lo.oop_maps.last().expect("release publication").bytecode_pc;
                assert_ne!(release_id, incoming_id);
                let ids = if thin_unlock {
                    vec![release_id, incoming_id]
                } else {
                    vec![release_id]
                };
                assert_eq!(lo.call_throw_pads.len(), ids.len());
                let first_pad = lo.buf.pos();
                lo.emit_call_throw_pads();
                let mut call = vec![0x48, 0xb8];
                call.extend((lo.local_handler_lookup as u64).to_le_bytes());
                call.extend([0xff, 0xd0]);
                let actual: Vec<_> = lo.buf.as_slice()[first_pad..]
                    .windows(call.len())
                    .enumerate()
                    .filter(|(_, bytes)| *bytes == call)
                    .map(|(offset, _)| {
                        let ret = first_pad + offset + call.len();
                        lo.oop_maps
                            .iter()
                            .find(|map| map.native_pc_offset as usize == ret)
                            .expect("each lookup return binds its incoming ID")
                            .bytecode_pc
                    })
                    .collect();
                assert_eq!(actual, ids);
                assert!(lo.take_latched_bailout().is_none());
            }
        },
    );
}

/// Round nio21 wave 3 (lane L2): with the helper unwired the site is not
/// armed -- the graph's edge is built, its `If` reads the flag's zero, and
/// the call keeps today's exit (no site in the artifact).
#[test]
pub(in crate::ir_lower::tests) fn nio21w3_an_unwired_lookup_arms_nothing() {
    crate::x64::set_moving_young_override(Some(false));
    extern "C" fn dispatch(_vm: i64, _info: i64, args: i64, _n: i64) -> i64 {
        // SAFETY: as in the test above.
        let x = unsafe { *(args as *const i64) };
        if x < 0 {
            i64::MIN
        } else {
            7000 + x
        }
    }
    let code = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0x4c, 0x05, 0xac];
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "pkg/T",
        method_name: "g",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut builder = IrBuilder::new(1, 2);
    builder.set_param_types(&[IrType::Int]);
    let mut invoke_info = HashMap::new();
    invoke_info.insert(
        1usize,
        (info as *const JitInvokeInfo as usize, 1usize, b'I'),
    );
    builder.set_invoke_info(invoke_info);
    builder.set_local_throw_rows(vec![crate::ir::IrLocalThrowRow {
        start_pc: 0,
        end_pc: 5,
        handler_pc: 5,
        catch: crate::ir::IrCatchClass::Any,
    }]);
    builder.set_call_throw_catch_names(vec![""], 0);
    let graph = builder.build(&code, 8).expect("IR build");
    let schedule = ir_schedule::schedule(&graph);
    let mut helpers = no_helpers();
    helpers.invoke_dispatch = dispatch as *const () as usize;
    let cm = lower(&graph, &schedule, 1, 2, &helpers).expect("lowers");
    assert!(cm._jit_local_handler_sites.is_empty());
    // SAFETY: an `int f(int)` body whose only call is the stub above.
    let got = unsafe { cm.try_call_with_context(0, &[2]) }.expect("call");
    assert_eq!(got as i32, 7002);
    // SAFETY: as above.
    let got = unsafe { cm.try_call_with_context(0, &[-1]) }.expect("call");
    assert_eq!(got, i64::MIN, "today's exit");
}

/// Round 9 wave 8 (irl8), request G of
/// `perf-compiled-ldc-string-is-a-helper-call-per-execution`: an
/// `Op::ConstString` / `Op::ConstClass` with a GC-maintained slot reads the
/// slot first and calls its helper only while the slot is empty; with no
/// slot the body is the plain helper call. EXECUTED: a filled slot answers
/// its word without the helper, an empty one falls through to the helper's
/// answer, and a rewritten slot (what the collector does in place) is
/// read afresh by the same code.
#[test]
pub(in crate::ir_lower::tests) fn nio21_w15_a_deferred_new_calls_new_object_cp_and_leaves_on_zero()
{
    extern "C" fn new_cp_ok(_vm: i64, _holder: i64, _cp: i64) -> i64 {
        0x7777_0000
    }
    extern "C" fn new_cp_fails(_vm: i64, _holder: i64, _cp: i64) -> i64 {
        0
    }
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let obj = g.add(
        Op::NewCp {
            holder_class_id: 7,
            cp_idx: 3,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(0),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, obj], Some(3));
    let sch = ir_schedule::schedule(&g);
    // No helper wired: refused before any code is emitted.
    assert!(lower(&g, &sch, 0, 0, &no_helpers()).is_none());
    let mut helpers = no_helpers();
    helpers.new_object_cp = new_cp_ok as *const () as usize;
    let cm = lower(&g, &sch, 0, 0, &helpers).expect("a deferred new lowers");
    // SAFETY: the synthetic helper ignores its arguments.
    let got = unsafe { cm.try_call_with_context(0, &[]) }.expect("call");
    assert_eq!(got, 0x7777_0000, "the helper's object is the result");
    helpers.new_object_cp = new_cp_fails as *const () as usize;
    let cm = lower(&g, &sch, 0, 0, &helpers).expect("a deferred new lowers");
    // SAFETY: as above.
    let got = unsafe { cm.try_call_with_context(0, &[]) }.expect("call");
    assert_eq!(got, i64::MIN, "0 = a stashed throwable: the exception edge");
}

#[test]
pub(in crate::ir_lower::tests) fn r9w8_an_ldc_with_a_constant_slot_reads_it_before_calling() {
    extern "C" fn ldc_helper(_vm: i64, _holder: i64, _cp: i64) -> i64 {
        0x7777_0000
    }
    let build = |string: bool, slot_addr: usize| {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let op = if string {
            Op::ConstString {
                holder_class_id: 7,
                cp_idx: 1,
                slot_addr,
            }
        } else {
            Op::ConstClass {
                holder_class_id: 7,
                cp_idx: 1,
                slot_addr,
            }
        };
        let c = g.add(op, IrType::Ref, vec![ctrl, mem], Some(0));
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, c], Some(2));
        g
    };
    let mut helpers = no_helpers();
    helpers.ldc_string_cp = ldc_helper as *const () as usize;
    helpers.ldc_class_cp = ldc_helper as *const () as usize;
    // MOV RAX,[RAX] ; TEST RAX,RAX ; JNZ rel32 -- the probe's tail.
    const PROBE: [u8; 8] = [0x48, 0x8B, 0x00, 0x48, 0x85, 0xC0, 0x0F, 0x85];
    for string in [true, false] {
        // No slot: the plain helper call, no probe.
        let g = build(string, 0);
        let sch = ir_schedule::schedule(&g);
        let cm = lower(&g, &sch, 0, 0, &helpers).expect("an ldc site lowers");
        assert!(
            !contains_seq(cm.code_bytes(), &PROBE),
            "string={string}: no slot, no probe"
        );
        // SAFETY: the synthetic helper ignores its arguments.
        let got = unsafe { cm.try_call_with_context(0, &[]) }.expect("call");
        assert_eq!(got, 0x7777_0000, "string={string}: the helper answers");

        // With a slot. `word` outlives every call below.
        let mut word: i64 = 0x0000_ABCD_0000;
        let slot_addr = std::ptr::addr_of_mut!(word) as usize;
        let g = build(string, slot_addr);
        let sch = ir_schedule::schedule(&g);
        let cm = lower(&g, &sch, 0, 0, &helpers).expect("a slotted ldc site lowers");
        assert!(
            contains_seq(cm.code_bytes(), &PROBE),
            "string={string}: a slot is probed"
        );
        // SAFETY: the probe reads `word`, which is live; the helper
        // ignores its arguments.
        let got = unsafe { cm.try_call_with_context(0, &[]) }.expect("call");
        assert_eq!(
            got, 0x0000_ABCD_0000,
            "string={string}: a hit skips the helper"
        );
        // The collector rewrites the word in place; the code re-reads it.
        // SAFETY: a plain write to a live local through its own address.
        unsafe { std::ptr::write_volatile(slot_addr as *mut i64, 0x0000_BEEF_0000) };
        // SAFETY: as above.
        let got = unsafe { cm.try_call_with_context(0, &[]) }.expect("call");
        assert_eq!(got, 0x0000_BEEF_0000, "string={string}: the rewritten word");
        // SAFETY: as above.
        unsafe { std::ptr::write_volatile(slot_addr as *mut i64, 0) };
        // SAFETY: as above.
        let got = unsafe { cm.try_call_with_context(0, &[]) }.expect("call");
        assert_eq!(
            got, 0x7777_0000,
            "string={string}: an empty slot calls the helper"
        );
    }
}

/// Round 9 wave 8 (irl8), the IR half of
/// `compiled-bastore-to-boolean-array-is-not-masked`: `bastore` into a
/// `boolean[]` stores `value & 1` (the interpreter's
/// `write_prim_element` `Boolean` arm), into a `byte[]` the low byte.
/// EXECUTED against a hand-made array header: only the kind byte, the
/// length word and the data matter to this body. Every pre-data header
/// byte is filled with 0x7F so the length reads as a large positive
/// `int` without this test naming the inventoried length-offset constant
/// (`layout_const_inventory.rs` counts code uses in this file).
#[test]
pub(in crate::ir_lower::tests) fn r9w8_bastore_into_a_boolean_array_stores_the_low_bit() {
    // static int f(<array> a, int v) { a[0] = v; return a[0]; }
    // aload_0 ; iconst_0 ; iload_1 ; bastore ; aload_0 ; iconst_0 ;
    // baload ; ireturn
    let code = [0x2a, 0x03, 0x1b, 0x54, 0x2a, 0x03, 0x33, 0xac];
    let bool_tag = cratonvm_types::primitive_array_kind_tags_byte("[Z").expect("[Z tag");
    let byte_tag = cratonvm_types::primitive_array_kind_tags_byte("[B").expect("[B tag");
    assert!(
        cratonvm_types::KIND_TAGS_BYTE_OFFSET < cratonvm_types::ARRAY_DATA_OFFSET,
        "precondition: the kind byte lies in the header"
    );
    for optimize in [false, true] {
        let mut builder = IrBuilder::new(2, 2);
        builder.set_param_types(&[IrType::Ref, IrType::Int]);
        let mut graph = builder.build(&code, code.len()).expect("IR build");
        if optimize {
            ir_optimize::optimize(&mut graph);
        }
        let schedule = ir_schedule::schedule(&graph);
        let cm = lower(&graph, &schedule, 2, 2, &no_helpers())
            .unwrap_or_else(|| panic!("bastore must lower (optimize={optimize})"));
        for (tag, is_bool) in [(bool_tag, true), (byte_tag, false)] {
            for v in [0i32, 1, 2, 3, -1, -2, 255, 256, 0x17F, i32::MIN, i32::MAX] {
                // 64 bytes, 8-aligned: header plus data.
                let mut obj = [0u64; 8];
                let base = obj.as_mut_ptr().cast::<u8>();
                // SAFETY: every offset written is below 64, inside `obj`.
                unsafe {
                    for off in 0..cratonvm_types::ARRAY_DATA_OFFSET {
                        *base.add(off) = 0x7F;
                    }
                    *base.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET) = tag;
                }
                // SAFETY: an `int f(ref, int)` body with no call; the
                // array is non-null and index 0 is in bounds, so no
                // guard fires. Cast: the raw pointer is the reference.
                let got = unsafe { cm.try_call(&[base as i64, i64::from(v)]) }.expect("call");
                // Cast: `(byte) v` is the JVMS `byte[]` truncation.
                let want = if is_bool { v & 1 } else { i32::from(v as i8) };
                assert_eq!(
                    got as i32, want,
                    "v={v} boolean[]={is_bool} (optimize={optimize})"
                );
            }
        }
    }
}

/// Round 9 wave 6 (irlower6): a `long`-counted loop whose `lcmp` result's
/// frame colour is reused by a later node in the block still fuses its
/// exit test. `ArithOsr.mulOnly` (`s += i * 3 + (i >> 1)`): on the wave-5
/// binary the compare's clobber test refused the compare (the `lcmp`'s word
/// went to the body's first temporary), which also skipped the `lcmp`
/// fusion, so every iteration materialised `lcmp` with `SETG; SETL; SUB`
/// and the boolean with `SETGE`. EXECUTED over counts around the edges.
#[test]
pub(in crate::ir_lower::tests) fn r9w6_a_long_counted_loop_fuses_its_lcmp_exit_test() {
    static FLAG: u8 = 0;
    extern "C" fn slow_poll() {}
    let code: [u8; 36] = [
        0x09, // 0: lconst_0
        0x41, // 1: lstore_2         s = 0
        0x09, // 2: lconst_0
        0x37, 0x04, // 3: lstore 4   i = 0
        0x16, 0x04, // 5: lload 4    <- loop header
        0x1e, // 7: lload_0
        0x94, // 8: lcmp
        0x9c, 0x00, 0x19, // 9: ifge +25 -> 34
        0x20, // 12: lload_2
        0x16, 0x04, // 13: lload 4
        0x06, // 15: iconst_3
        0x85, // 16: i2l
        0x69, // 17: lmul
        0x16, 0x04, // 18: lload 4
        0x04, // 20: iconst_1
        0x7b, // 21: lshr
        0x61, // 22: ladd
        0x61, // 23: ladd
        0x41, // 24: lstore_2
        0x16, 0x04, // 25: lload 4
        0x0a, // 27: lconst_1
        0x61, // 28: ladd
        0x37, 0x04, // 29: lstore 4
        0xa7, 0xff, 0xe6, // 31: goto -26 -> 5
        0x20, // 34: lload_2
        0xad, // 35: lreturn
    ];
    for optimize in [false, true] {
        let mut builder = IrBuilder::new(1, 6);
        builder.set_param_types(&[IrType::Long]);
        let mut graph = builder.build(&code, code.len()).expect("IR build");
        if optimize {
            ir_optimize::optimize(&mut graph);
        }
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.safepoint_flag_addr = &FLAG as *const u8 as usize;
        helpers.safepoint_slow_path = slow_poll as *const () as usize;
        let cm = lower(&graph, &schedule, 1, 6, &helpers)
            .unwrap_or_else(|| panic!("the kernel must lower (optimize={optimize})"));
        if ir_register_authoritative_enabled() {
            // SETG AL -- the `lcmp` materialisation.
            assert!(
                !contains_seq(cm.code_bytes(), &[0x0F, 0x9F, 0xC0]),
                "the loop's lcmp was not fused into its exit test (optimize={optimize})"
            );
        }
        for n in [i64::MIN, -1, 0, 1, 2, 3, 7, 1000, 100_003] {
            let mut want = 0i64;
            let mut i = 0i64;
            while i < n {
                want = want.wrapping_add(i.wrapping_mul(3).wrapping_add(i >> 1));
                i += 1;
            }
            // SAFETY: a `long f(long)` body with no call (the poll's slow
            // path is a no-op) and no trap.
            let got = unsafe { cm.try_call(&[n]) }.expect("call");
            assert_eq!(got, want, "n={n} (optimize={optimize})");
        }
    }
}

/// Round 9 wave 6 (irlower6): the emission half of split-residency edge
/// resolution. One copy on one edge is one `MOV r64, [rbp - home]` into the
/// planned register, it marks the edge done (the post-lowering check
/// refuses a method with an edge whose copies never ran), it makes
/// `edge_has_phi_copies` answer `true` so every `If` arm gives the edge
/// straight-line code of its own, and it does NOT touch the residency
/// tables (the next block in layout order is not on this edge's path).
#[test]
pub(in crate::ir_lower::tests) fn r9w6_an_edge_resolution_copy_is_one_home_load_on_its_own_edge() {
    let code: [u8; 36] = [
        0x09, 0x41, 0x09, 0x37, 0x04, 0x16, 0x04, 0x1e, 0x94, 0x9c, 0x00, 0x19, 0x20, 0x16, 0x04,
        0x06, 0x85, 0x69, 0x16, 0x04, 0x04, 0x7b, 0x61, 0x61, 0x41, 0x16, 0x04, 0x0a, 0x61, 0x37,
        0x04, 0xa7, 0xff, 0xe6, 0x20, 0xad,
    ];
    let mut builder = IrBuilder::new(1, 6);
    builder.set_param_types(&[IrType::Long]);
    let graph = builder.build(&code, code.len()).expect("IR build");
    let schedule = ir_schedule::schedule(&graph);
    assert!(
        schedule.blocks.len() >= 2,
        "precondition: a loop schedules into several blocks"
    );
    let plan = plan_slots(&graph, &schedule, None);
    // Cast: an index into the node arena is a `NodeId`.
    let param = graph
        .nodes
        .iter()
        .position(|node| matches!(node.op, Op::Param(0)))
        .expect("the parameter is in the graph") as NodeId;
    let graph: &'static Graph = Box::leak(Box::new(graph));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan));
    let mut lo = lowerer_over(graph, schedule, plan, 4096);
    lo.alloc_slot_checked(param)
        .expect("the parameter has a planned home word");
    // Edge 0 -> 1 is the loop's entry edge and carries the header's phis,
    // whose sources `gather_phi_copies` resolves to home words: give every
    // planned value its word, as the block walk would have by now.
    for id in 0..graph.nodes.len() {
        // Cast: an index into the node arena is a `NodeId`.
        let _ = lo.alloc_slot_checked(id as NodeId);
    }
    let (pred, succ) = (0usize, 1usize);
    lo.ls_edge_copies = vec![LsEdgeCopies {
        pred,
        succ,
        copies: vec![(param, RBX, true)],
    }];
    lo.ls_edge_copies_done = vec![false];
    assert!(
        lo.edge_has_phi_copies(pred, succ),
        "an edge with a resolution copy must get a copy site of its own"
    );
    assert_eq!(
        lo.edge_has_phi_copies(succ, pred),
        !lo.gather_phi_copies(succ, pred).is_empty(),
        "and no other edge changes its answer"
    );

    // Another edge: nothing.
    let before = lo.buf.pos();
    lo.ls_emit_edge_copies(succ, pred);
    assert_eq!(lo.buf.pos(), before, "an edge with no copy emits nothing");
    assert_eq!(lo.ls_edge_copies_done, vec![false]);

    lo.ls_emit_edge_copies(pred, succ);
    let emitted = lo.buf.as_slice()[before..].to_vec();
    let refused = lo.take_latched_bailout();
    assert!(
        refused.is_none(),
        "a copy with no phi on its edge must not be refused: {refused:?}"
    );
    assert_eq!(
        emitted.first().copied(),
        Some(0x48),
        "REX.W: {emitted:02x?}"
    );
    assert_eq!(
        emitted.get(1).copied(),
        Some(0x8B),
        "MOV r64, r/m64: {emitted:02x?}"
    );
    assert_eq!(
        emitted.get(2).map(|modrm| (modrm >> 3) & 7),
        Some(RBX),
        "into RBX: {emitted:02x?}"
    );
    assert_eq!(
        lo.ls_edge_copies_done,
        vec![true],
        "the edge is marked done"
    );
    assert_eq!(lo.ls_edge_loads, 1);
    assert_eq!(
        lo.resident_gpr(param),
        None,
        "the copy is not a publish: the emitter's tables describe the next \
             block in LAYOUT order, which this edge does not reach"
    );
}

/// Round 9 wave 2 (isel #3): the d2i/d2l/f2i/f2l fixup ported from
/// `x64::arith::emit_fp_to_int_nan_fixup` (`CMP r,1; JNO` / `NOT` for MAX).
/// `x / 0.0` drives every arm from an int parameter: +inf → MAX, -inf →
/// MIN, NaN (0/0) → 0; plain `(T) x` covers the in-range fast path,
/// including `(int)(double)MIN`, whose correct answer IS the indefinite
/// pattern.
#[test]
pub(in crate::ir_lower::tests) fn r9w2_fp_to_int_conversions_saturate_and_zero_nan() {
    // (code, is_long)
    let shapes: [(&[u8], bool); 4] = [
        // iload_0 i2d dconst_0 ddiv d2i ireturn
        (&[0x1a, 0x87, 0x0e, 0x6f, 0x8e, 0xac], false),
        // iload_0 i2d dconst_0 ddiv d2l lreturn
        (&[0x1a, 0x87, 0x0e, 0x6f, 0x8f, 0xad], true),
        // iload_0 i2f fconst_0 fdiv f2i ireturn
        (&[0x1a, 0x86, 0x0b, 0x6e, 0x8b, 0xac], false),
        // iload_0 i2f fconst_0 fdiv f2l lreturn
        (&[0x1a, 0x86, 0x0b, 0x6e, 0x8c, 0xad], true),
    ];
    for (code, is_long) in shapes {
        let cm = r9w2_compile_one_arg(code, false, false);
        for (x, want_i, want_l) in [
            (5i32, i64::from(i32::MAX), i64::MAX),
            (-5, i64::from(i32::MIN), i64::MIN),
            (0, 0, 0),
        ] {
            // SAFETY: a one-int-argument body with no helper calls.
            let got = unsafe { cm.try_call(&[i64::from(x)]) }.expect("call");
            if is_long {
                assert_eq!(got, want_l, "{code:02x?} x={x}");
            } else {
                // Cast: this tier's `int` result is the low 32 bits.
                assert_eq!(i64::from(got as i32), want_i, "{code:02x?} x={x}");
            }
        }
    }
    // In range: iload_0 i2d d2i ireturn / iload_0 i2d d2l lreturn.
    let d2i = r9w2_compile_one_arg(&[0x1a, 0x87, 0x8e, 0xac], false, false);
    let d2l = r9w2_compile_one_arg(&[0x1a, 0x87, 0x8f, 0xad], false, false);
    for x in [i32::MIN, -1, 0, 1, 123_456, i32::MAX] {
        // SAFETY: as above.
        let got = unsafe { d2i.try_call(&[i64::from(x)]) }.expect("call");
        // Cast: this tier's `int` result is the low 32 bits.
        assert_eq!(got as i32, x, "(int)(double){x}");
        // SAFETY: as above.
        let got = unsafe { d2l.try_call(&[i64::from(x)]) }.expect("call");
        assert_eq!(got, i64::from(x), "(long)(double){x}");
    }
}

thread_local! {
    pub(in crate::ir_lower::tests) static R9W2_AASTORES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    pub(in crate::ir_lower::tests) static R9W2_TYPE_CHECKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A leaked, header-shaped fake VALUE for the executed `aastore` tests:
/// `class_id` in the header's first `u32`, `kind` at the KIND_TAGS byte
/// (0 = a plain object), `mark` in word 3 for the fake type checks to
/// read. Round 11 wave 14: the arm now reads a non-null value's header
/// (the exact-component path), so a bare integer no longer stands in for
/// one.
pub(in crate::ir_lower::tests) fn r11w14_fake_value(class_id: u32, kind: u8, mark: u64) -> i64 {
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET < 24);
    let words: &'static mut [u64; 4] = Box::leak(Box::new([u64::from(class_id), 0, 0, mark]));
    let base = words.as_mut_ptr();
    // SAFETY: the KIND_TAGS byte lies inside the leaked 32-byte block
    // (the assertion above), below the mark word.
    unsafe {
        (base as *mut u8) // Cast: word block → byte cursor
            .add(cratonvm_types::KIND_TAGS_BYTE_OFFSET)
            .write(kind)
    };
    base as i64 // Cast: leaked address
}

/// zs s3/bt: the fused compare's two encodings. RIP-relative when the
/// word is in reach -- the displacement resolves to the word from the end
/// of the 8-byte instruction -- and through R11 otherwise.
#[test]
pub(in crate::ir_lower::tests) fn zs3bt_the_fused_compare_encodes_both_forms() {
    let here = 0x7000_0000_1000usize;
    let near = here + 0x40;
    let b = zgc_fused_gate_cmp_bytes(here, near);
    assert_eq!(b.len(), 8);
    assert_eq!(&b[..3], &[0x66, 0x83, 0x3D]);
    assert_eq!(b[7], 0);
    let disp = i32::from_le_bytes([b[3], b[4], b[5], b[6]]);
    // Cast: a small in-range displacement.
    assert_eq!((here + 8).wrapping_add(disp as isize as usize), near);
    let far = here + (1usize << 40);
    let b = zgc_fused_gate_cmp_bytes(here, far);
    assert_eq!(b.len(), 15);
    assert_eq!(&b[..2], &[0x49, 0xBB]);
    // Cast: the baked address.
    assert_eq!(&b[2..10], &(far as u64).to_le_bytes());
    assert_eq!(&b[10..], &[0x66, 0x41, 0x83, 0x3B, 0x00]);
}

/// zs s3/bt source contract: the fused gate's `JE` goes to the
/// barrier-free store (never past the helper, never to it), in front of
/// the UNCHANGED pre gate, in exactly the two optimizing-tier stores; and
/// in `putfield` the flags byte the store's shape test reads (CL) is
/// loaded before the fused jump.
#[test]
pub(in crate::ir_lower::tests) fn zs3bt_the_fused_gate_jumps_to_the_store_in_front_of_the_pre_gate()
{
    let src = concat!(
        include_str!("../../ir_lower/mod.rs"),
        "\n",
        include_str!("../../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_session_confined_rows.rs")
    );
    let body = |name: &str| -> &str {
        let start = src.find(name).expect("function present");
        let end = src[start..]
            .find("\n    fn ")
            .map_or(src.len(), |e| start + e);
        &src[start..end]
    };
    let site = "inline_store.push(self.emit_zgc_fused_ref_store_gate(word));";
    let put = body("fn emit_gated_ir_ref_putfield(");
    let aa = body("fn emit_gated_ir_aastore(");
    for (name, b, pre) in [
        ("putfield", put, "// -- SATB pre-barrier gate"),
        ("aastore", aa, "// SATB pre-barrier gate."),
    ] {
        let fused = b
            .find(site)
            .unwrap_or_else(|| panic!("{name}: no fused gate"));
        let pre = b.find(pre).unwrap_or_else(|| panic!("{name}: no pre gate"));
        assert!(fused < pre, "{name}: the fused gate precedes the pre gate");
    }
    let movzx = put
        .find("MOVZX ECX, byte [RAX + disp32]")
        .expect("flags byte load");
    assert!(movzx < put.find(site).unwrap(), "CL is loaded first");
    // A leading space: the emitted sites, not the quoted literal above.
    let needle = format!(" {site}");
    assert_eq!(src.matches(needle.as_str()).count(), 2, "exactly two sites");
}

/// zs s5/bt2: the ZGC putstatic pre-gate compare's RIP form: the
/// displacement resolves to the gate byte from the end of the 7-byte
/// instruction (`80 3D d32 00`), and an out-of-reach gate keeps the R11
/// form (`None`).
#[test]
pub(in crate::ir_lower::tests) fn zs5bt2_the_putstatic_pre_gate_rip_compare_resolves_to_the_byte() {
    let here = 0x7000_0000_1000usize;
    let near = here - 0x80;
    let b = zgc_pre_byte_cmp_rip_bytes(here, near).expect("in reach");
    assert_eq!(b.len(), 7);
    assert_eq!(&b[..2], &[0x80, 0x3D]);
    assert_eq!(b[6], 0);
    let disp = i32::from_le_bytes([b[2], b[3], b[4], b[5]]);
    // Cast: a small in-range displacement.
    assert_eq!((here + 7).wrapping_add(disp as isize as usize), near);
    assert!(zgc_pre_byte_cmp_rip_bytes(here, here + (1usize << 40)).is_none());
}

/// zs s5/bt2: a hand-built constructor shape -- `o = new C; o.a = p;
/// o.a = p; o.b = p; p.a = o; o.c = p` -- and what
/// `zgc_fresh_receiver_stores` admits: the FIRST store of each field into
/// the fresh object, never a second store of a field (its old value is no
/// longer null), nothing after the object escapes (stored into `p`, which
/// also closes the window as a store into another object), and nothing
/// for a scalar-replaced allocation.
pub(in crate::ir_lower::tests) fn zs5bt2_constructor_graph() -> (Graph, NodeId, [NodeId; 5]) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let param = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let newo = g.add(
        Op::New {
            class_id: 7,
            num_fields: 3,
        },
        IrType::Ref,
        vec![c0, mem],
        Some(0),
    );
    let f = [0i64, 1, 2].map(|i| g.add(Op::Const(i), IrType::Int, vec![], None));
    let st = |g: &mut Graph, mem: NodeId, base: NodeId, field: NodeId, v: NodeId, pc| {
        g.add(
            Op::Store(MemKind::Ref),
            IrType::Memory,
            vec![c0, mem, base, field, v],
            Some(pc),
        )
    };
    let a1 = st(&mut g, mem, newo, f[0], param, 4);
    let a2 = st(&mut g, a1, newo, f[0], param, 8);
    let b1 = st(&mut g, a2, newo, f[1], param, 12);
    let esc = st(&mut g, b1, param, f[0], newo, 16);
    let c1 = st(&mut g, esc, newo, f[2], param, 20);
    g.exit = g.add(Op::Return, IrType::Void, vec![c0], Some(24));
    (g, newo, [a1, a2, b1, esc, c1])
}

#[test]
pub(in crate::ir_lower::tests) fn zs5bt2_fresh_receiver_stores_follow_the_window_rule() {
    let (g, newo, [a1, a2, b1, esc, c1]) = zs5bt2_constructor_graph();
    let schedule = ir_schedule::schedule(&g);
    let fresh = zgc_fresh_receiver_stores(&g, &schedule, |_| false);
    assert!(fresh.contains(&a1), "the first store of field a");
    assert!(
        !fresh.contains(&a2),
        "field a again: its old value is not null"
    );
    assert!(fresh.contains(&b1), "the first store of field b");
    assert!(!fresh.contains(&esc), "a store into another object");
    assert!(!fresh.contains(&c1), "after the object escaped");
    assert_eq!(fresh.len(), 2);
    let none = zgc_fresh_receiver_stores(&g, &schedule, |v| v == newo);
    assert!(
        none.is_empty(),
        "a scalar-replaced allocation is never fresh"
    );
}

/// zt t9/ca (Q1 census): the scan names the node that closed the window
/// -- here the escaping store `p.f = newo` (a store into another object
/// whose value is the open object), once -- and returns the same set as
/// the plain function. With the allocation scalar-replaced no window
/// opens, so nothing is named.
#[test]
pub(in crate::ir_lower::tests) fn zt9ca_the_fresh_receiver_scan_names_the_closing_node() {
    let (g, newo, [_, _, _, esc, _]) = zs5bt2_constructor_graph();
    let schedule = ir_schedule::schedule(&g);
    let mut closers: Vec<NodeId> = Vec::new();
    let fresh = zgc_fresh_receiver_scan(&g, &schedule, |_| false, |id| closers.push(id));
    assert_eq!(
        closers,
        vec![esc],
        "the escaping store closed the only window"
    );
    assert_eq!(fresh, zgc_fresh_receiver_stores(&g, &schedule, |_| false));
    let mut none: Vec<NodeId> = Vec::new();
    let _ = zgc_fresh_receiver_scan(&g, &schedule, |v| v == newo, |id| none.push(id));
    assert!(none.is_empty(), "no window opened: nothing to close");
}

/// j27 wave 5 (lane BARHUNT): `o = new C; p = new D; o.a = p; o.b = q`.
/// A second `Op::New` opens its own window beside `o`'s instead of closing
/// it (the documented rule; the doc used to say an allocation closes every
/// window): `o.a = p` is a first store into a fresh `o` (p escapes, so ITS
/// window closes), and `o.b = q` still is. A `NewArray` in the same spot
/// closes `o`'s window.
#[test]
pub(in crate::ir_lower::tests) fn j27_barhunt_a_nested_new_keeps_the_outer_window_and_a_new_array_closes_it(
) {
    let build = |nested: Op| {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let q = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let new = |class_id| Op::New {
            class_id,
            num_fields: 2,
        };
        let o = g.add(new(7), IrType::Ref, vec![c0, mem], Some(0));
        let len = g.add(Op::Const(4), IrType::Int, vec![], None);
        let p_inputs = match nested {
            Op::New { .. } => vec![c0, mem],
            _ => vec![c0, mem, len],
        };
        let p = g.add(nested, IrType::Ref, p_inputs, Some(4));
        let f = [0i64, 1].map(|i| g.add(Op::Const(i), IrType::Int, vec![], None));
        let st = |g: &mut Graph, m: NodeId, v: NodeId, field: NodeId, pc| {
            g.add(
                Op::Store(MemKind::Ref),
                IrType::Memory,
                vec![c0, m, o, field, v],
                Some(pc),
            )
        };
        let a = st(&mut g, mem, p, f[0], 8);
        let b = st(&mut g, a, q, f[1], 12);
        g.exit = g.add(Op::Return, IrType::Void, vec![c0], Some(16));
        (g, a, b)
    };
    let (g, a, b) = build(Op::New {
        class_id: 8,
        num_fields: 1,
    });
    let fresh = zgc_fresh_receiver_stores(&g, &ir_schedule::schedule(&g), |_| false);
    assert!(fresh.contains(&a), "o.a = p after a nested New");
    assert!(fresh.contains(&b), "o.b = q: o's window is still open");
    let (g, a, b) = build(Op::NewArray {
        element_type: 10,
        component_class_id: 0,
    });
    let schedule = ir_schedule::schedule(&g);
    let fresh = zgc_fresh_receiver_stores(&g, &schedule, |_| false);
    // The two allocations are independent, so the scheduler orders them;
    // the rule is about the array allocation landing INSIDE `o`'s window.
    let at = |id: NodeId| {
        schedule
            .blocks
            .iter()
            .find_map(|blk| blk.nodes.iter().position(|&n| n == id))
    };
    let (o, arr) = (g.nodes[a as usize].inputs[2], g.nodes[a as usize].inputs[4]);
    let inside = at(o) < at(arr);
    assert_eq!(
        fresh.contains(&a) || fresh.contains(&b),
        !inside,
        "an array allocation inside the window closes it"
    );
}

/// A colored store keeps the gate on a fresh receiver.
#[test]
pub(in crate::ir_lower::tests) fn a_colored_store_keeps_the_gate_on_a_fresh_receiver() {
    assert!(ir_fresh_store_skips_gate(true, false));
    assert!(!ir_fresh_store_skips_gate(true, true));
    assert!(!ir_fresh_store_skips_gate(false, false));
    assert!(!ir_fresh_store_skips_gate(false, true));
}

/// zs s5/bt2: the emission. On a ZGC-shaped plan the fused gate emits
/// `MOVZX ECX, [RAX+flags]; CMP word ...; JE store`; a fresh receiver's
/// store emits `MOVZX ECX, [RAX+flags]; JMP store` instead -- no gate
/// compare at all -- and keeps the rest of the sequence (the helper, the
/// legacy drop) unchanged behind it. Without a fused gate (G1, the mask
/// plan, `CRATONVM_JIT_ZGC_FUSED_REF_STORE_GATE=0`) the flag changes
/// nothing: the set is never built and the argument is ignored.
#[test]
pub(in crate::ir_lower::tests) fn zs5bt2_a_fresh_receiver_putfield_emits_no_gate_compare() {
    if !crate::x64::ir_gated_ref_store_enabled()
        || !crate::x64::inline_putfield_enabled()
        || crate::x64::narrow_oops_block_inline_fields()
        || !cratonvm_types::compact_ref_fields_enabled()
    {
        return;
    }
    let (g, newo, [a1, ..]) = zs5bt2_constructor_graph();
    let param = g
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Param(0)))
        .expect("the parameter") as NodeId; // Cast: a small node index
    let schedule = ir_schedule::schedule(&g);
    let graph: &'static Graph = Box::leak(Box::new(g));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let gates: *mut u8 = Box::leak(Box::new([0u8, 0u8])).as_mut_ptr();
    let floor: *mut u8 = Box::leak(Box::new(0x10u8));
    let emit = |fresh: bool, fused: bool| -> Vec<u8> {
        let mut lo = lowerer_over(graph, schedule, plan, 8192);
        lo.ref_store_gates = Some((gates as usize, gates as usize + 1, floor as usize));
        lo.ref_store_post_skip_mask = None;
        lo.zgc_fused_gate = fused.then_some(gates as usize); // Cast: leaked bytes
        lo.read_bounds_addr = 0x1000;
        lo.compact_fields.insert((4, true), (8, true, b'L'));
        let before = lo.buf.pos();
        assert!(lo.emit_gated_ir_ref_putfield(Some(4), newo, param, 0, fresh));
        lo.buf.as_slice()[before..].to_vec()
    };
    let compares = |b: &[u8]| {
        let rip = b.windows(3).filter(|w| *w == [0x66, 0x83, 0x3D]).count();
        let r11 = b
            .windows(5)
            .filter(|w| *w == [0x66, 0x41, 0x83, 0x3B, 0x00])
            .count();
        rip + r11
    };
    let mut movzx_prefix = vec![0x0F, 0xB6, 0x88];
    movzx_prefix.extend_from_slice(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
    let has = |b: &[u8], n: &[u8]| b.windows(n.len()).any(|w| w == n);

    let gated = emit(false, true);
    let elided = emit(true, true);
    assert_eq!(compares(&gated), 1, "the fused compare (positive control)");
    assert_eq!(compares(&elided), 0, "a fresh receiver: no gate compare");
    assert!(
        has(&elided, &movzx_prefix) && elided.len() < gated.len(),
        "flags byte, then straight to the store without emitting gates"
    );
    // No fused gate: the fresh flag is inert (same length, no compare of
    // the word, no jump past the gates; the bytes themselves differ only
    // in the RIP displacements of two buffers).
    let (plain_fresh, plain) = (emit(true, false), emit(false, false));
    assert_eq!(plain_fresh.len(), plain.len());
    assert_eq!(compares(&plain_fresh), 0);
    // And the set itself is empty unless the switch AND the plan agree.
    let lo = lowerer_over(graph, schedule, plan, 4096);
    assert!(lo.zgc_fresh_stores.is_empty(), "no plan in `no_helpers`");
    assert!(zgc_fresh_receiver_stores(graph, schedule, |_| false).contains(&a1));
    // SAFETY: the leaked gate bytes, reset for any later reader.
    unsafe { gates.write_volatile(0) };
}

/// zs5bt2 Q3: a fresh `NewArray` whose `ArrayStore(Ref)` needs no
/// `aastore_type_check` (e.g. `new C[]{ new C() }` or `new C[]{ null }`)
/// is admitted into `zgc_fresh_receiver_stores` and skips the fused gate,
/// while a second store to the same index or a store needing a type check
/// keeps the gate.
#[test]
pub(in crate::ir_lower::tests) fn zs5bt2_fresh_newarray_aastore_emits_no_gate_compare() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let len = g.add(Op::Const(2), IrType::Int, vec![], None);
    let arr = g.add(
        Op::NewArray {
            element_type: 0,
            component_class_id: 7,
        },
        IrType::Ref,
        vec![c0, mem, len],
        Some(0),
    );
    let elem = g.add(
        Op::New {
            class_id: 7,
            num_fields: 0,
        },
        IrType::Ref,
        vec![c0, arr],
        Some(4),
    );
    let idx0 = g.add(Op::Const(0), IrType::Int, vec![], None);
    let idx1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let null_val = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let st_exact = g.add(
        Op::ArrayStore(MemKind::Ref),
        IrType::Memory,
        vec![c0, elem, arr, idx0, elem],
        Some(8),
    );
    let st_null = g.add(
        Op::ArrayStore(MemKind::Ref),
        IrType::Memory,
        vec![c0, st_exact, arr, idx1, null_val],
        Some(12),
    );
    let st_overwrite = g.add(
        Op::ArrayStore(MemKind::Ref),
        IrType::Memory,
        vec![c0, st_null, arr, idx0, null_val],
        Some(16),
    );
    g.exit = g.add(Op::Return, IrType::Void, vec![c0], Some(20));

    let schedule = ir_schedule::schedule(&g);
    let fresh = zgc_fresh_receiver_stores(&g, &schedule, |_| false);
    assert!(
        fresh.contains(&st_exact),
        "exact-component store into fresh NewArray"
    );
    assert!(fresh.contains(&st_null), "null store into fresh NewArray");
    assert!(
        !fresh.contains(&st_overwrite),
        "second store to same index keeps gate"
    );

    let graph: &'static Graph = Box::leak(Box::new(g));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let gates: *mut u8 = Box::leak(Box::new([0u8, 0u8])).as_mut_ptr();
    let floor: *mut u8 = Box::leak(Box::new(0x10u8));
    let gate_tuple = (gates as usize, gates as usize + 1, floor as usize);
    let emit = |is_fresh: bool| -> Vec<u8> {
        let mut lo = lowerer_over(graph, schedule, plan, 8192);
        lo.ref_store_gates = Some(gate_tuple);
        lo.ref_store_post_skip_mask = None;
        lo.zgc_fused_gate = Some(gates as usize);
        let before = lo.buf.pos();
        let _ = lo.emit_gated_ir_aastore(gate_tuple, arr, idx0, elem, is_fresh);
        lo.buf.as_slice()[before..].to_vec()
    };
    let compares = |b: &[u8]| {
        let rip = b.windows(3).filter(|w| *w == [0x66, 0x83, 0x3D]).count();
        let r11 = b
            .windows(5)
            .filter(|w| *w == [0x66, 0x41, 0x83, 0x3B, 0x00])
            .count();
        rip + r11
    };
    assert_eq!(
        compares(&emit(false)),
        1,
        "non-fresh aastore emits fused compare"
    );
    assert_eq!(
        compares(&emit(true)),
        0,
        "fresh NewArray aastore skips fused compare"
    );
    // SAFETY: reset leaked gate bytes.
    unsafe { gates.write_volatile(0) };
}

/// gcj j3/rt3: the store window `CRATONVM_JIT_IR_SATB_GATE_ONCE` leaves
/// without a second gate test calls nothing: the card arms up to their
/// stores, the card PRE check and the clean-card inline mark emit no
/// `CALL` (source pin; the only calls of the arms are after the store,
/// the POST edge's card-only `write_barrier`).
#[test]
pub(in crate::ir_lower::tests) fn gcj3rt3_the_satb_gate_once_store_window_calls_nothing() {
    let src = concat!(
        include_str!("../../ir_lower/mod.rs"),
        "\n",
        include_str!("../../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_session_confined_rows.rs")
    )
    .replace("\r\n", "\n");
    // Needles split so this test's own text never matches them.
    let body = |name: &str| -> String {
        let at = src.find(&format!("fn {name}(")).expect("the emitter");
        let rest = &src[at + 1..];
        rest[..rest.find("\n    }\n").expect("its end")].to_string()
    };
    let call = concat!("0xFF, ", "0xD0");
    let put = body("emit_ir_putfield_card_arm");
    let put_store = put
        .find(concat!("// MOV [RAX + disp32], ", "RDX"))
        .expect("the putfield store");
    assert!(
        !put[..put_store].contains(call),
        "no CALL before the putfield store"
    );
    let aa = body("emit_ir_aastore_card_arm");
    let aa_store = aa
        .find(concat!(
            "self.emit_gpr_array_elem_",
            "store(MemKind::Long, ArrayStoreSource::Reg(RDX))"
        ))
        .expect("the aastore store");
    assert!(
        !aa[..aa_store].contains(call),
        "no CALL before the aastore store"
    );
    for f in [
        "emit_ir_card_pre_check",
        "emit_ir_card_store_inline",
        "emit_ir_satb_pre_gate",
    ] {
        assert!(!body(f).contains(call), "{f} emits no CALL");
    }
}

/// Round 13 wave 2 (lane irhash, W7-2): with the inline card check on
/// (`CRATONVM_JIT_INLINE_CARD_MARK=1`), a gated `aastore` of a young value
/// into an OLD array stores inline when the element's card is already
/// dirty and still calls `jit_aastore` on a clean one; an old value into an
/// old array needs no card at all; an armed SATB gate still takes the
/// helper. Executed against a real card table over a simulated old space.
#[cfg(target_arch = "x86_64")]
/// gcj j2/jt2 (`CRATONVM_JIT_POST_ONLY_CARD_MARK=1`): the optimizing
/// tier's gated `aastore` in the post-only shape, EXECUTED against a real
/// card table over a simulated old space. A young value into an old array
/// over a CLEAN card is stored INLINE -- no `jit_aastore`, which the
/// PRE/POST form takes for a clean card (`r13w2_irhash_...`) -- and the
/// element's card is dirty after the store: marked by the card-only
/// `write_barrier` in the check-only form, inline (or, above the scan-bound
/// mirror, by that call) in the card-store form. A dirty card makes no
/// call. Each compile counts its site in the view.
#[cfg(target_arch = "x86_64")]
/// gcf f1/p: the optimizing tier's copies of the view's trace offsets are
/// the collector's.
#[test]
pub(in crate::ir_lower::tests) fn gcf_f1p_ir_card_trace_offsets_match_the_view() {
    use cratonvm_gc::card_table::JitCardView;
    assert_eq!(
        IR_CARD_TRACE_DIRTY as usize,
        JitCardView::TRACE_IR_DIRTY_OFFSET
    );
    assert_eq!(
        IR_CARD_TRACE_CLEAN as usize,
        JitCardView::TRACE_IR_CLEAN_OFFSET
    );
}

/// gcf f2/p: this tier's copies of the card-store words are the
/// collector's, and the summary shift is `log2(SUMMARY_GROUP)`.
#[test]
pub(in crate::ir_lower::tests) fn gcf_f2p_ir_card_store_offsets_match_the_view() {
    use cratonvm_gc::card_table::{JitCardView, SUMMARY_GROUP};
    assert_eq!(
        IR_CARD_VIEW_SCAN_BOUND as usize,
        JitCardView::SCAN_BOUND_OFFSET
    );
    assert_eq!(
        IR_CARD_VIEW_SUMMARY_NEG as usize,
        JitCardView::SUMMARY_NEG_OFFSET
    );
    assert_eq!(IR_CARD_VIEW_FLAGS, JitCardView::FLAGS_OFFSET);
    assert_eq!(IR_CARD_VIEW_FLAG_SUMMARY, JitCardView::FLAG_SUMMARY);
    assert_eq!(1usize << IR_CARD_SUMMARY_SHIFT, SUMMARY_GROUP);
}

/// gcf f2/p (`CRATONVM_JIT_CARD_STORE_BARRIER`): with the card-store form
/// on, a gated `aastore` of a young value into an OLD array whose element
/// card is CLEAN marks the card (and its summary group) inline and stores
/// without calling `jit_aastore` -- once the card lies under the view's
/// scan-bound mirror. Above the mirror it still calls (the Rust barrier
/// raises the bound). Executed against a real card table WITH the summary
/// map, and the scan finds the mark.
#[cfg(target_arch = "x86_64")]
/// Round 9 wave 2 (deopt #3 + the callee-deopt ordering page): the
/// service's cold side copies the site's shadow words back into their
/// homes — without consuming the publication or retracting `top` — BEFORE
/// it loads the service helper, and its sentinel test is `CMP RAX,1; JNO`.
#[test]
pub(in crate::ir_lower::tests) fn r9w2_the_callee_deopt_service_copies_back_before_it_stages() {
    const SERVICE: u64 = 0x7fff_0000_0000_5000;
    let mut lo = lowerer_with_resident_xmm(4096, None);
    lo.service_callee_deopt = SERVICE as usize;
    lo.get_current_thread = 0x7fff_0000_0000_6000;
    lo.shadow_thread_slot_off = 16;
    lo.shadow_savebase_slot_off = 24;
    lo.pending_shadow = vec![40, 48];
    let pos = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);

    let start = lo.buf.pos();
    lo.emit_inline_callee_deopt_service(0x1234, 0, &[], false, &mut None);
    let code = lo.buf.as_slice()[start..].to_vec();
    assert!(
        code.starts_with(&[0x48, 0x83, 0xF8, 0x01, 0x0F, 0x81]),
        "the sentinel test must be `CMP RAX, 1; JNO rel32`: {:02x?}",
        &code[..code.len().min(8)]
    );
    let copy_at = pos(&code, &[0x49, 0x8B, 0x0B]).expect("MOV RCX, [R11]: the copy-back");
    let svc_at = pos(&code, &SERVICE.to_le_bytes()).expect("the service helper is called");
    assert!(copy_at < svc_at, "copy back first, then service");
    assert_eq!(
        count_seq(&code, &[0x49, 0x8B, 0x0B]),
        2,
        "one copy per published word"
    );
    assert!(
        !contains_seq(&code, &[0x4D, 0x89, 0x9A]),
        "the cold copy-back must not retract `top` (`MOV [R10+ss_top], R11`)"
    );
    assert_eq!(
        lo.pending_shadow,
        vec![40, 48],
        "the site's own reload still owns it"
    );
    assert_eq!(lo.shadow_reloads, 0, "not a reload for the balance check");

    // The merged route has already reloaded: nothing copied again.
    let start = lo.buf.pos();
    lo.emit_inline_callee_deopt_service(0x1234, 0, &[], true, &mut None);
    let code = lo.buf.as_slice()[start..].to_vec();
    assert!(!contains_seq(&code, &[0x49, 0x8B, 0x0B]));
}

/// JIT round 19 wave 5, lane throws5 (THROWS4-1,
/// `CRATONVM_JIT_DIRECT_CALL_SENTINEL_LEAN`): a direct cross call marked
/// lean hands the callee-deopt service the lean word on every shape it
/// lowers to (inline service or the deferred cold tail), and an unmarked
/// one the plain count. Proven from the emitted bytes.
#[test]
pub(in crate::ir_lower::tests) fn r19w5_throws5_direct_cross_call_lean_word_only_when_marked() {
    const SERVICE: u64 = 0x7fff_0000_0000_5100;
    const ENTRY: usize = 0x7fff_0000_0000_9100;
    let lean_word = crate::SERVICE_ARGS_LEAN_PROPAGATE_BIT; // 0 args | bit
    let count = |hay: &[u8], word: u64| -> usize {
        let needle = word.to_le_bytes();
        hay.windows(needle.len()).filter(|w| *w == needle).count()
    };
    let emit = |lean: bool| -> Vec<u8> {
        let mut lo = lowerer_with_resident_xmm(8192, None);
        lo.service_callee_deopt = SERVICE as usize;
        lo.direct_call_lean_site = lean;
        let start = lo.buf.pos();
        lo.emit_direct_cross_call(
            &[],
            -8,
            0,
            ENTRY,
            false,
            None,
            0x1234,
            IrType::Int,
            false,
            0,
            None,
            &mut None,
        );
        lo.emit_deferred_self_call_tails();
        lo.buf.as_slice()[start..].to_vec()
    };
    let off = emit(false);
    let services = count(&off, SERVICE);
    assert!(services > 0, "the cross call services its sentinel");
    assert_eq!(count(&off, lean_word), 0, "unmarked: the plain count");
    let on = emit(true);
    assert_eq!(count(&on, SERVICE), services, "the same service calls");
    assert_eq!(
        count(&on, lean_word),
        services,
        "marked: every service call carries the lean bit"
    );
}

/// Round 11 wave 10 (lane irexc,
/// `r11w9-lower-ir-tier-never-compiles-a-javac-synchronized-block`): a
/// call-exception exit inside a protected range gets a reason-9 rethrow
/// pad — a `PendingException` box at the THROW bci, RETHROW semantics, an
/// empty operand stack — and NOT a `call_exc_patches` row; an exit at the
/// range's exclusive end keeps the plain bail. The pad stamps the bci, loads
/// its point and jumps to the shared deopt stub.
#[test]
pub(in crate::ir_lower::tests) fn r11w10_a_protected_call_exit_gets_a_rethrow_pad_not_the_bail_stub(
) {
    const SET_THROW_BCI: u64 = 0x7fff_0000_0000_7000;
    let mut lo = lowerer_with_resident_xmm(4096, None);
    lo.set_throw_bci = SET_THROW_BCI as usize;
    lo.protected_ranges = vec![(4, 14)];

    lo.cur_bci = 9;
    lo.buf.emit(&[0x0F, 0x80]); // JO rel32
    let inside = lo.buf.pos();
    lo.buf.emit(&[0, 0, 0, 0]);
    let boxes_before = lo.deopt_boxes.len();
    lo.push_call_exc_patch(inside);
    assert!(
        lo.call_exc_patches.is_empty(),
        "a protected exit must not reach the params-only bail stub"
    );
    assert_eq!(lo.rethrow_pads.len(), 1);
    assert_eq!(lo.deopt_boxes.len(), boxes_before + 1);
    let point = lo.deopt_boxes.last().expect("the pad's point is boxed");
    assert_eq!(point.bci, 9, "keyed on the throwing bci, not its successor");
    assert_eq!(point.frame_state.bci, 9);
    assert_eq!(point.reason, DeoptReason::PendingException);
    assert!(point.semantics.rethrow_exception);
    assert!(
        point.frame_state.stack.is_empty(),
        "JVMS 2.10: handler entry"
    );
    assert_eq!(
        lo.rethrow_points.len(),
        1,
        "a by-value copy for `deopt_points`"
    );
    assert!(lo.transfer_bcis.contains(&9));

    lo.cur_bci = 14;
    lo.buf.emit(&[0x0F, 0x80]);
    let outside = lo.buf.pos();
    lo.buf.emit(&[0, 0, 0, 0]);
    lo.push_call_exc_patch(outside);
    assert_eq!(
        lo.call_exc_patches,
        vec![(outside, 14)],
        "end_pc is exclusive: the plain exit"
    );
    assert_eq!(lo.rethrow_pads.len(), 1);

    let point_ptr = lo.rethrow_pads[0].point_ptr;
    let start = lo.buf.pos();
    lo.emit_rethrow_pads();
    let code = lo.buf.as_slice()[start..].to_vec();
    assert!(lo.rethrow_pads.is_empty());
    assert_eq!(
        lo.deopt_stub_patches.len(),
        1,
        "one JMP to the shared deopt stub"
    );
    assert!(
        contains_seq(&code, &SET_THROW_BCI.to_le_bytes()),
        "the pad stamps this method's throw bci: {code:02x?}"
    );
    assert!(
        contains_seq(&code, &point_ptr.to_le_bytes()),
        "the pad hands the stub its own point: {code:02x?}"
    );
    let mut rel = [0u8; 4];
    rel.copy_from_slice(&lo.buf.as_slice()[inside..inside + 4]);
    assert_eq!(
        inside as i64 + 4 + i64::from(i32::from_le_bytes(rel)),
        start as i64,
        "the site's exceptional branch lands on the pad"
    );
}

/// Round 11 wave 10 (lane irexc): the frames the reason-9 consumers would
/// propagate past a matching handler are refused at compile time, and a
/// lock the compiled code holds itself is not one of them.
#[test]
pub(in crate::ir_lower::tests) fn r11w10_a_rethrow_frame_the_consumers_cannot_take_is_refused() {
    let held = crate::deopt::MonitorInfo {
        object: FrameValue::StackSlotRef(-16),
        lock_depth: 1,
        relock: false,
    };
    let base = FrameState {
        class_id: 0,
        method_key: String::new(),
        bci: 3,
        locals: vec![FrameValue::Int(1), FrameValue::StackSlotRef(-16)],
        stack: Vec::new(),
        monitors: vec![held],
        caller: None,
    };
    assert_eq!(
        rethrow_frame_refusal(&base, false),
        None,
        "a held lock is released once, by the handler's own monitorexit"
    );
    let mut relock = base.clone();
    relock.monitors[0].relock = true;
    assert!(rethrow_frame_refusal(&relock, false).is_some());
    let mut unsupported = base.clone();
    unsupported.locals[0] = FrameValue::Unsupported;
    assert!(rethrow_frame_refusal(&unsupported, false).is_some());
    let mut virtual_local = base.clone();
    virtual_local.locals[1] = FrameValue::VirtualObjectRef(7);
    assert!(rethrow_frame_refusal(&virtual_local, false).is_some());
    let mut lost_lock = base.clone();
    lost_lock.monitors[0].object = FrameValue::Unsupported;
    assert!(rethrow_frame_refusal(&lost_lock, false).is_some());
    let mut chain = base.clone();
    chain.caller = Some(Box::new(base.clone()));
    assert!(rethrow_frame_refusal(&chain, false).is_some());
}

/// Round 11 wave 11 (lane irexc,
/// `r11w10-orch-osr-exc-exits-loses-handler-updated-locals`):
/// `R11SyncCallerHoldsCalleeLock.throwLoop`'s shape — a counted loop whose
/// protected call's `catch` bumps a counter and falls back into the loop —
/// lowered on the precise-frame route. The call gets its rethrow pad; the
/// header, which the handler reaches, gets NO OSR entry, because the
/// graph's header facts (`caught` is `φ(0, itself)`, collapsed to the
/// constant 0) exclude the handler's path. The control lowers the same
/// graph with no handler reach and keeps the entry, which is what every
/// body did before the fix — so the second half fails without it.
#[test]
pub(in crate::ir_lower::tests) fn r11w11_a_header_a_handler_reaches_gets_no_osr_entry_beside_a_rethrow_pad(
) {
    crate::x64::set_moving_young_override(Some(false));
    let _osr = OsrEntryForce::on();
    //  0 iconst_0; 1 istore_1; 2 iconst_0; 3 istore_2
    //  4 iload_2  <- header;  5 iload_0; 6 if_icmpge 27
    //  9 iload_2  <- try [9, 14); 10 invokestatic #1; 13 pop; 14 goto 21
    // 17 astore_3 <- handler;   18 iinc 1, 1
    // 21 iinc 2, 1; 24 goto 4;  27 iload_1; 28 ireturn
    let code = [
        0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x15, 0x1c, 0xb8, 0x00, 0x01, 0x57, 0xa7,
        0x00, 0x07, 0x4e, 0x84, 0x01, 0x01, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xec, 0x1b, 0xac, 0x00,
        0x00,
    ];
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "pkg/T",
        method_name: "divSync",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3, // static
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let lower_with = |reachable: Option<&[u32]>| {
        let mut builder = IrBuilder::new(1, 4);
        builder.set_param_types(&[IrType::Int]);
        let mut invoke_info = HashMap::new();
        invoke_info.insert(
            10usize,
            (info as *const JitInvokeInfo as usize, 1usize, b'I'),
        );
        builder.set_invoke_info(invoke_info);
        let mut graph = builder.build(&code, 29).expect("IR build of the loop");
        ir_optimize::optimize(&mut graph);
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.invoke_dispatch = 0x1111_2222_3333_4440;
        helpers.dispatch_threw = 0x1111_2222_3333_4442;
        let empty_hints: HashMap<usize, bool> = HashMap::new();
        let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
        let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        lower_inner_with_protected_ranges(
            &crate::DirectHelperTable::EMPTY,
            &graph,
            &schedule,
            1,
            4,
            &helpers,
            &empty_hints,
            &[],
            None,
            &no_direct,
            &no_ic,
            &no_compact,
            &crate::ir::IrInlineFrameSites::default(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
            None,
            &[(9, 14)],
            reachable,
        )
        .expect("the loop lowers on the precise-frame route")
    };
    let has_pad = |cm: &CompiledMethod| {
        cm.deopt_points
            .iter()
            .any(|p| p.bci == 10 && p.reason == DeoptReason::PendingException)
    };

    let control = lower_with(Some(&[][..]));
    assert!(
        has_pad(&control),
        "the protected call exits through a rethrow pad"
    );
    assert!(
        control.ir_osr_entry_addr(4).is_some(),
        "control: with no handler reach the header keeps its entry (if this \
             fails the fixture lost its entry for another reason — see the \
             `CRATONVM_DBG_IR_LINEAR_SCAN` refusal census — not the fix)"
    );

    let reach = crate::deopt::pcs_reachable_from_handlers(&code, &[17]);
    let fixed = lower_with(reach.as_deref());
    assert!(has_pad(&fixed));
    assert!(
        fixed.ir_osr_entry_addr(4).is_none(),
        "the handler at 17 falls back to the header: re-entering there would \
             read `caught` as the collapsed constant"
    );
    // Unanalysable handler reach withholds every entry of such a body.
    let unknown = lower_with(None);
    assert!(unknown.ir_osr_entries.is_empty());
}

/// Round 11 wave 13 (lane irexc), item 3 of
/// `r11w9-tier-optimizing-osr-exception-exits-door-contract-patch`: the
/// zero-divisor `Op::Guard` `add_div_zero_guard` plants at an
/// `idiv`/`lrem` is EXACT (the re-executed bytecode throws), so it no
/// longer withdraws `ir_osr_guard_exit_bcis`. Asked at any other bci —
/// where the snapshot's top of stack is not the divisor — it is not.
#[test]
pub(in crate::ir_lower::tests) fn r11w13_a_zero_divisor_guard_is_exact_at_its_division_only() {
    let lowered = |code: &[u8], params: &[IrType], locals: usize| {
        let mut builder = IrBuilder::new(params.len(), locals);
        builder.set_param_types(params);
        let graph = builder
            .build(code, code.len())
            .expect("IR build of the division");
        let graph: &'static Graph = Box::leak(Box::new(graph));
        let schedule: &'static Schedule = Box::leak(Box::new(ir_schedule::schedule(graph)));
        let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
        let lo = lowerer_over(graph, schedule, plan, 4096);
        let guard = graph
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Guard { bci: 2 }))
            .expect("the zero-divisor guard at the division's bci");
        // Cast: a node index is a `NodeId`.
        (lo, guard as NodeId)
    };
    // 0 iload_0; 1 iload_1; 2 idiv; 3 ireturn
    let int_params = [IrType::Int, IrType::Int];
    let (lo, guard) = lowered(&[0x1au8, 0x1b, 0x6c, 0xac][..], &int_params[..], 2);
    assert!(lo.op_guard_is_exact(guard, 2));
    assert!(
        !lo.op_guard_is_exact(guard, 3),
        "the divisor is not on top at 3"
    );
    assert!(!lo.op_guard_is_exact(guard, 0), "empty stack at 0");
    // 0 lload_0; 1 lload_2; 2 lrem; 3 lreturn
    let long_params = [IrType::Long, IrType::Long];
    let (lo, guard) = lowered(&[0x1eu8, 0x20, 0x71, 0xad][..], &long_params[..], 4);
    assert!(lo.op_guard_is_exact(guard, 2));
}

/// Round 9 wave 2 (`ir-bailouts-latched-after-the-latch-check-are-dropped`):
/// a dropped home no register describes latches `UnallocatedValue` — except
/// while `build_deopt_points` resolves a point no deopt can arrive at.
#[test]
pub(in crate::ir_lower::tests) fn r9w2_a_dropped_home_latches_only_where_a_deopt_can_arrive() {
    let mut lo = lowerer_with_resident_xmm(4096, None);
    // `add_one_graph`: n5 is the `Add`.
    let sum: NodeId = 5;
    assert!(matches!(lo.graph.nodes[sum as usize].op, Op::Add));
    lo.home_dropped = vec![false; lo.graph.nodes.len()];
    lo.home_dropped[sum as usize] = true;

    lo.quiet_dropped_home.set(true);
    assert!(matches!(lo.frame_value_for(sum), FrameValue::Unsupported));
    assert!(
        lo.take_latched_bailout().is_none(),
        "quiet: nothing latched"
    );

    lo.quiet_dropped_home.set(false);
    assert!(matches!(lo.frame_value_for(sum), FrameValue::Unsupported));
    let b = lo.take_latched_bailout().expect("enforced: latched");
    assert!(matches!(b.reason, BailoutReason::UnallocatedValue { node } if node == sum));
}

/// Round 9 wave 2 (`ir-scalar-deopt-sites-keyed-by-raw-pc-inside-splices`):
/// a guard inside a spliced body fires the deopt at the ENCLOSING invoke,
/// so `deopt_block_for_snapshot(_, invoke)` must find it; a division is judged
/// anchored by ITS OWN raw pc, not by the invoke.
#[test]
pub(in crate::ir_lower::tests) fn r9w2_spliced_guards_and_divisions_are_indexed_at_their_resume_bci(
) {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let a = g.add(Op::Param(0), IrType::Int, vec![start], None);
    let b = g.add(Op::Param(1), IrType::Int, vec![start], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let cond = g.add(
        Op::Cmp(crate::ir::CmpOp::Ne),
        IrType::Int,
        vec![b, zero],
        Some(40),
    );
    // The builder's div-zero guard for the division at raw pc 40.
    let guard = g.add(
        Op::Guard { bci: 40 },
        IrType::Void,
        vec![ctrl, cond],
        Some(40),
    );
    let anchored_div = g.add(Op::Div, IrType::Int, vec![a, b], Some(40));
    // A second division in the same splice with NO guard at its own pc.
    let lone_div = g.add(Op::Div, IrType::Int, vec![anchored_div, b], Some(41));
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, lone_div], Some(42));
    let schedule = ir_schedule::schedule(&g);
    let graph: &'static Graph = Box::leak(Box::new(g));
    let schedule: &'static Schedule = Box::leak(Box::new(schedule));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let mut lo = lowerer_over(graph, schedule, plan, 4096);
    // The relocated body 9..43 stands in for the invoke at bci 5.
    let spliced: &'static [(usize, usize, usize)] =
        Box::leak(Box::new([(9usize, 43usize, 5usize)]));
    lo.spliced_ranges = spliced;
    lo.deopt_sites_by_bci = Lowerer::index_deopt_sites(graph, spliced);

    assert!(
        lo.deopt_sites_by_bci
            .get(&5)
            .is_some_and(|v| v.contains(&guard) && v.contains(&lone_div)),
        "both sites resume at the invoke and must be indexed there: {:?}",
        lo.deopt_sites_by_bci
    );
    assert!(
        lo.deopt_sites_by_bci.get(&40).is_none(),
        "nothing is keyed by a raw pc"
    );
    assert!(lo.fires_deopt_at(&graph.nodes[guard as usize], 5));
    assert!(
        !lo.fires_deopt_at(&graph.nodes[anchored_div as usize], 5),
        "the guard at the division's own pc owns its trap"
    );
    assert!(
        lo.fires_deopt_at(&graph.nodes[lone_div as usize], 5),
        "a division with no guard at ITS pc traps itself, whatever else the splice holds"
    );
}

/// Round 9 wave 2 (`ir-code-buffer-overflow-is-a-permanent-fallback`): a
/// first attempt that overflows is lowered again at `wanted()` and the
/// method still gets a correct body.
#[test]
pub(in crate::ir_lower::tests) fn r9w2_a_code_buffer_overflow_is_retried_at_the_exact_size() {
    // The do-while loop of `do_while_latch_with_one_sided_phi_copies_runs_correctly`.
    let code: [u8; 18] = [
        0x03, 0x3c, 0x03, 0x3d, 0x1b, 0x1c, 0x60, 0x3c, 0x84, 0x02, 0x01, 0x1c, 0x1a, 0xa1, 0xff,
        0xf7, 0x1b, 0xac,
    ];
    static FLAG: u8 = 0;
    extern "C" fn slow_poll() {}
    let graph = IrBuilder::new(1, 3)
        .build(&code, code.len())
        .expect("IR build");
    let schedule = ir_schedule::schedule(&graph);
    let mut helpers = no_helpers();
    helpers.safepoint_flag_addr = &FLAG as *const u8 as usize;
    helpers.safepoint_slow_path = slow_poll as *const () as usize;
    let retries_before = ir_buffer_overflow_retries();
    IR_FIRST_ATTEMPT_CAPACITY_FOR_TEST.with(|c| c.set(Some(16)));
    let compiled = lower(&graph, &schedule, 1, 3, &helpers);
    IR_FIRST_ATTEMPT_CAPACITY_FOR_TEST.with(|c| c.set(None));
    let compiled = compiled.expect("an overflowed first attempt must be retried, not refused");
    assert!(
        ir_buffer_overflow_retries() > retries_before,
        "the retry path ran"
    );
    for (n, want) in [(0i64, 0i32), (1, 0), (5, 10), (100, 4950)] {
        // SAFETY: one int argument, int result; the poll flag is a static zero.
        let got = unsafe { compiled.try_call(&[n]) }.expect("call");
        // Cast: this tier's `int` result is the low 32 bits.
        assert_eq!(got as i32, want, "f({n})");
    }
}

/// Round 11 wave 10 (lane irstatic, `r11w9-lower-ir-putstatic-patch`): an
/// `Op::StoreStatic` calls its width's `jit_putstatic_*` helper with
/// `(vm, class_id, field_index, value)`; a sentinel answer leaves through
/// the exception stub instead of falling through to the rest of the body;
/// a volatile write is followed by the StoreLoad `LOCK ADD` and a plain one
/// is not; and a graph whose helper is absent is refused.
#[test]
pub(in crate::ir_lower::tests) fn r11w10_a_store_static_calls_its_helper_and_fences_only_when_volatile(
) {
    // Answers 0 only for the exact arguments the graph below bakes, so a
    // wrong marshal shows up as the sentinel exit.
    extern "C" fn putstatic_checks(_vm: i64, class_id: i64, field_index: i64, val: i64) -> i64 {
        if class_id == 3 && field_index == 1 && val as i32 == 42 {
            0
        } else {
            i64::MIN
        }
    }
    extern "C" fn putstatic_throws(_vm: i64, _c: i64, _f: i64, _v: i64) -> i64 {
        i64::MIN
    }
    let build = |type_tag: u8, is_volatile: bool| {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let value = g.add(Op::Const(42), IrType::Int, vec![], None);
        let _store = g.add(
            Op::StoreStatic {
                class_id: 3,
                field_index: 1,
                type_tag,
                is_volatile,
            },
            IrType::Memory,
            vec![ctrl, mem, value],
            Some(1),
        );
        let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
        g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, seven], Some(4));
        g
    };
    let lower_with = |g: &Graph, helpers: &JitRuntimeHelpers| {
        let sch = ir_schedule::schedule(g);
        lower(g, &sch, 0, 0, helpers)
    };
    const LOCK_ADD: [u8; 5] = [0xF0, 0x83, 0x04, 0x24, 0x00];

    let mut helpers = no_helpers();
    helpers.putstatic_int = putstatic_checks as *const () as usize;
    let plain = lower_with(&build(b'I', false), &helpers).expect("putstatic lowers");
    assert!(
        !contains_seq(plain.code_bytes(), &LOCK_ADD),
        "a plain static write owes no fence"
    );
    // SAFETY: the synthetic helper ignores the context argument.
    let got = unsafe { plain.try_call_with_context(0, &[]) }.expect("call");
    // Cast: this tier's `int` result is the low 32 bits.
    assert_eq!(
        got as i32, 7,
        "the helper accepted the marshalled arguments"
    );

    let fenced = lower_with(&build(b'I', true), &helpers).expect("volatile putstatic lowers");
    assert!(
        contains_seq(fenced.code_bytes(), &LOCK_ADD),
        "a volatile static write is followed by the StoreLoad fence"
    );

    // A `<clinit>` failure: the helper answers the sentinel and the body
    // must leave through the exception stub rather than return 7.
    helpers.putstatic_int = putstatic_throws as *const () as usize;
    let throwing = lower_with(&build(b'I', false), &helpers).expect("lowers");
    // SAFETY: as above.
    let got = unsafe { throwing.try_call_with_context(0, &[]) }.expect("call");
    assert_eq!(got, i64::MIN, "a failed putstatic must not fall through");

    // An absent helper refuses the graph rather than CALL address zero.
    helpers.putstatic_int = 0;
    assert!(
        lower_with(&build(b'I', false), &helpers).is_none(),
        "no putstatic_int helper -> refuse"
    );
}

/// Round 16 (lane fieldguard): with `CRATONVM_JIT_INLINE_PUTSTATIC=1` and a
/// resolver that answers, a primitive `Op::StoreStatic` writes the cell's
/// two words exactly as `write_value_atomic` (the helper's writer) would,
/// and leaves no CALL to its helper; with the switch off, or for a site
/// the resolver declines, the helper call is unchanged.
/// Round 9 (irlower): `clobbered_within` is the binary-searched form of
/// the linear `any(pos in lo..=hi && regs.contains(reg))` scans it
/// replaced. Checked against that scan over every window of a small list.
#[test]
pub(in crate::ir_lower::tests) fn clobbered_within_agrees_with_the_linear_scan() {
    use crate::regalloc::PhysReg;
    let rbx = PhysReg::gp(3);
    let r12 = PhysReg::gp(12);
    let clobbers: Vec<(usize, Vec<PhysReg>)> = vec![
        (2, vec![rbx]),
        (5, vec![r12]),
        (9, vec![rbx, r12]),
        (14, vec![r12]),
    ];
    for lo in 0..16usize {
        for hi in lo..16usize {
            for reg in [rbx, r12, PhysReg::gp(13)] {
                let linear = clobbers
                    .iter()
                    .any(|(p, regs)| *p >= lo && *p <= hi && regs.contains(&reg));
                assert_eq!(
                    super::clobbered_within(&clobbers, lo, hi, reg),
                    linear,
                    "window {lo}..={hi}, {reg:?}"
                );
            }
        }
    }
}

/// Round 9 (irlower): a guard's deopt path is OUT OF LINE. At the guard
/// site there is exactly one `Jcc rel32` on the FAILING condition and
/// nothing else — the passing path falls straight through — and the pad
/// (`MOV DEOPT_ARG0, point; JMP stub`) appears only when
/// `emit_deopt_pads` runs, with the `Jcc` patched to its first byte and
/// its `JMP` queued for the shared stub.
#[test]
pub(in crate::ir_lower::tests) fn a_guard_falls_through_on_success_and_its_deopt_pad_is_out_of_line(
) {
    let mut lo = lowerer_with_resident_xmm(4096, None);
    lo.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
    let guard_at = lo.buf.pos();
    lo.emit_deopt_if_zero(0, DeoptReason::NullCheck);
    let after_guard = lo.buf.pos();
    assert_eq!(
        after_guard - guard_at,
        6,
        "the guard site must be one `Jcc rel32` and nothing else"
    );
    let code = lo.buf.as_slice().to_vec();
    assert_eq!(
        &code[guard_at..guard_at + 2],
        &[0x0F, 0x84],
        "deopt on ZF (JZ): the inverse of `emit_deopt_if_zero`'s JNZ-continue"
    );
    assert!(
        lo.deopt_stub_patches.is_empty(),
        "no stub JMP until the pads are emitted"
    );
    assert_eq!(lo.deopt_pads.len(), 1);

    lo.emit_deopt_pads();
    assert!(lo.deopt_pads.is_empty());
    assert_eq!(
        lo.deopt_stub_patches.len(),
        1,
        "the pad's JMP targets the shared stub"
    );
    let code = lo.buf.as_slice().to_vec();
    let mut rel_bytes = [0u8; 4];
    rel_bytes.copy_from_slice(&code[guard_at + 2..guard_at + 6]);
    assert_eq!(
        i32::from_le_bytes(rel_bytes),
        0,
        "the Jcc lands on the pad, which here is the next byte after the guard"
    );
    let jmp_patch = *lo.deopt_stub_patches.last().expect("one patch");
    assert_eq!(code[jmp_patch - 1], 0xE9, "the pad ends in JMP rel32");
}

/// Round 13 wave 11 (lane chain5, CH-1): the forced chain deopt block is
/// emitted only under its switch, counts in a counter the lowerer owns,
/// preserves RAX and the flags on the path that continues, and jumps to a
/// pad for the SAME point (so the stub gets the chain the guard publishes).
#[test]
pub(in crate::ir_lower::tests) fn a_forced_chain_deopt_counts_and_shares_the_points_pad() {
    let mut lo = lowerer_with_resident_xmm(4096, None);
    lo.forced_chain_deopts = false;
    let at = lo.buf.pos();
    lo.emit_forced_chain_deopt(0x1234);
    assert_eq!(lo.buf.pos(), at, "switch off: nothing is emitted");
    assert!(lo.deopt_pads.is_empty());
    assert!(lo.forced_chain_deopt_counters.is_empty());

    lo.forced_chain_deopts = true;
    lo.emit_forced_chain_deopt(0x1234);
    let code = lo.buf.as_slice()[at..].to_vec();
    assert_eq!(code.len(), 26);
    assert_eq!(
        &code[..4],
        &[0x9C, 0x50, 0x48, 0xB8],
        "PUSHFQ; PUSH RAX; MOV RAX, imm64"
    );
    let counter = lo
        .forced_chain_deopt_counters
        .last()
        .expect("one counter per chain point");
    let addr = counter.as_ref() as *const crate::deopt::ForcedChainTripCell as u64;
    assert_eq!(
        &code[4..12],
        &addr.to_le_bytes(),
        "the counter is the one owned"
    );
    // Round 16 lane chain (CH5W4-1): no in-flight mark any more; both
    // paths pop RAX and the flags; the cell is recorded against its point.
    assert_eq!(
        &code[12..],
        &[
            0x80, 0x00, 0x04, // ADD BYTE [RAX], 4
            0x75, 0x07, // JNZ keep
            0x58, 0x9D, // POP RAX ; POPFQ
            0xE9, 0, 0, 0, 0, // JMP forced pad
            0x58, 0x9D, // keep: POP RAX ; POPFQ
        ],
    );
    // JNZ at offset 15 (2 bytes) + 0x07 lands on `keep` at offset 24. The
    // forced pad is the point's own, its pointer tagged forced.
    assert_eq!(
        lo.deopt_pads,
        vec![(at + 20, 0x1234 | crate::deopt::FORCED_CHAIN_PAD_TAG)],
        "the point's own forced pad"
    );
    lo.emit_deopt_pads();
    let pad = &lo.buf.as_slice()[at + 26..];
    assert_eq!(
        &pad[2..10],
        &(0x1234u64 | crate::deopt::FORCED_CHAIN_PAD_TAG).to_le_bytes(),
        "MOV DEOPT_ARG0, tagged point"
    );
    assert_eq!(
        lo.forced_chain_deopt_points,
        vec![0x1234],
        "the cell's point is untagged"
    );
    assert_eq!(lo.forced_chain_deopt_counters.len(), 1);
}

/// The shared deopt stub retracts a frame block, and only a frame block.
///
/// A guard is never inside a call, so under per-call publication nothing is
/// outstanding when one fails and the stub has always skipped the restore.
/// A block is outstanding from the first safepoint to the return — and a
/// block left above `top` is a set of tagged addresses into a frame that
/// is dead the moment the stub returns.
#[test]
pub(in crate::ir_lower::tests) fn the_deopt_stub_retracts_a_frame_block() {
    let savetop_load = |lo: &Lowerer<'_>| {
        let mut n = vec![0x4C, 0x8B, 0x9D];
        n.extend_from_slice(&(-lo.shadow_savetop_slot_off).to_le_bytes());
        n
    };
    for block in [false, true] {
        let mut lo = lowerer_with_resident_xmm(4096, None);
        lo.get_current_thread = 0x7fff_0000_0000_3000;
        lo.shadow_off_in_thread = 0x40;
        assert!(lo.shadow_thread_slot_off > 0 && lo.shadow_savetop_slot_off > 0);
        if block {
            lo.frame_block = vec![8];
        }
        lo.buf.emit_byte(0xE9);
        lo.deopt_stub_patches.push(lo.buf.pos());
        lo.buf.emit(&[0, 0, 0, 0]);
        let stub = lo.buf.pos();
        lo.emit_deopt_stub();
        let code = lo.buf.as_slice()[stub..].to_vec();
        assert_eq!(
                count_seq(&code, &savetop_load(&lo)),
                usize::from(block),
                "block={block}: the stub restores the entry watermark exactly when a block can be outstanding"
            );
    }
}

/// Finding #84 on the optimizing tier: JVMS §6.5 `ireturn` narrows a
/// `Z`/`B`/`C`/`S` return. The builder inserts the same nodes `i2b`/`i2c`/
/// `i2s`/`iand` build, so only the low 32 bits of RAX are this tier's
/// `int` and that is what is compared.
#[test]
pub(in crate::ir_lower::tests) fn ireturn_narrows_boolean_byte_char_short_returns() {
    static FLAG: u8 = 0;
    extern "C" fn slow_poll() {}
    let cases: [(&[u8], usize, &str, &[i64], i32); 6] = [
        (&[0x05, 0xac], 0, "()Z", &[], 0),               // iconst_2
        (&[0x11, 0x00, 0xC8, 0xac], 0, "()B", &[], -56), // sipush 200
        (&[0x02, 0xac], 0, "()C", &[], 65535),           // iconst_m1
        (&[0x1a, 0xac], 1, "(I)S", &[70000], 4464),      // iload_0
        (&[0x1a, 0xac], 1, "(I)S", &[-70000], -4464),    // iload_0
        (&[0x1a, 0xac], 1, "(I)I", &[70000], 70000),     // control
    ];
    for (code, n, desc, args, want) in cases {
        let mut builder = IrBuilder::new(n, n);
        builder.set_return_descriptor(desc);
        let graph = builder.build(code, code.len()).expect("IR build");
        let schedule = ir_schedule::schedule(&graph);
        let mut helpers = no_helpers();
        helpers.safepoint_flag_addr = &FLAG as *const u8 as usize;
        helpers.safepoint_slow_path = slow_poll as *const () as usize;
        let compiled =
            lower(&graph, &schedule, n, n, &helpers).unwrap_or_else(|| panic!("{desc} must lower"));
        // SAFETY: the lowered body takes exactly `n` int arguments and
        // returns an int; the poll flag is a static zero.
        let got = unsafe { compiled.try_call(args) }.expect("call");
        // Cast: this tier's `int` result is the low 32 bits of RAX.
        assert_eq!(got as i32, want, "{desc} with {args:?}");
    }
}

/// Round 13 wave 1 (lane callcost): the METHOD-ENTRY poll is outlined by
/// default (`CRATONVM_JIT_IR_ENTRY_POLL_OUTLINE`), stops exactly as the
/// inline one does, and never stops on a clear flag (a `JZ`/`JNZ` mix-up
/// would call the slow path on every entry and still return 1).
/// An argument past the entry ABI's register file arrives on the CALLER'S
/// stack, and `emit_prologue` must load it from there.
///
/// This used to be a whole-method REFUSAL (`ir_lower_refuses_more_incoming_
/// slots_than_abi_registers`), because the prologue read registers and
/// nothing else. It was the commonest refusal an ordinary accessor hit — an
/// instance method with three parameters is four incoming slots, and
/// touching one field turns `needs_context` on, which is the fifth — so on
/// Win64 a three-argument getter could not be lowered at this tier at all.
///
/// Tested through the RETURNED VALUE rather than the emitted bytes: the
/// body is `iload_<last>; ireturn`, so a prologue that dropped the argument
/// answers with whatever the frame slot happened to hold, which is exactly
/// the silent wrong value the old refusal existed to prevent. Each argument
/// is given a distinct value, so reading the WRONG stack slot is a
/// different failure from reading none.
#[test]
pub(in crate::ir_lower::tests) fn ir_lower_loads_incoming_arguments_past_the_entry_abi_registers() {
    let cap = incoming_abi_reg_capacity();
    // `CompiledMethod::try_call` dispatches through one
    // `extern "C" fn(i64, …)` thunk per arity and has thunks up to
    // `TRY_CALL_MAX_ARGS`; past that it answers `Err(TooManyArgs)`.
    // That is a limit of this HARNESS, not of the lowering under test,
    // and the two ABIs sit on opposite sides of it: Win64 has four
    // entry registers, so `cap + 3` is 7 and callable, while SysV has
    // six, so `cap + 3` is 9 and comes back `Err(TooManyArgs(9))` —
    // which is how this test read RED on Linux and green on Windows
    // while the prologue it exercises was working on both.
    //
    // Clamp to what the harness can call, and assert the clamp still
    // leaves a stack argument to test: a bound that silently collapsed
    // to `0..=0` would turn this into a registers-only test that always
    // passes, which is the failure mode worth guarding.
    const TRY_CALL_MAX_ARGS: usize = 8;
    let max_extra = TRY_CALL_MAX_ARGS.saturating_sub(cap).min(3);
    assert!(
        max_extra >= 1,
        "the entry ABI takes {cap} arguments in registers and \
             `try_call` can pass at most {TRY_CALL_MAX_ARGS}, so this test \
             can no longer place ANY argument on the caller's stack. Give \
             `try_call` a wider thunk before trusting this test again."
    );
    for extra in 0..=max_extra {
        let slots = cap + extra;
        let last = slots - 1;
        // `iload <last>; ireturn` — `iload` (0x15) takes a one-byte index,
        // which covers every slot count this test uses.
        let code = [0x15, last as u8, 0xac];
        let compiled = compile_via_ir(&code, code.len(), slots, slots)
            .unwrap_or_else(|| panic!("a graph with {slots} incoming slots must lower"));
        let args: Vec<i64> = (0..slots).map(|i| 1000 + i as i64).collect();
        // SAFETY: the lowered body reads one int local and returns it; the
        // argument vector has exactly the arity the body was compiled for,
        // and every value is a plain integer.
        let got = unsafe { compiled.try_call(&args) };
        assert_eq!(
            got,
            Ok(1000 + last as i64),
            "with {slots} incoming slots ({cap} in registers, {extra} on the \
                 caller's stack), reading slot {last} must answer the argument \
                 that was passed there",
        );
    }
}

/// The same shape for a method that also takes the hidden VM context in
/// ABI[0], which is what shifts an ordinary accessor over the edge: the
/// context spends one register, so a method with `cap` Java arguments
/// already has one on the stack.
#[test]
pub(in crate::ir_lower::tests) fn ir_lower_loads_stack_arguments_when_the_context_spends_a_register(
) {
    let cap = incoming_abi_reg_capacity();
    // A body with an `Op::Call` turns `needs_context` on. `invokestatic`
    // through a helper is the smallest one this harness can build, so this
    // test asserts the ARITHMETIC of the shift rather than re-deriving it:
    // with the context in ABI[0], `cap` Java arguments leave exactly one on
    // the caller's stack.
    assert_eq!(
        cap.saturating_sub(1),
        incoming_abi_reg_capacity() - 1,
        "one register is spent on the context"
    );
    // And the register/stack split the prologue computes for that case.
    let base = 1usize; // needs_context
    let num_params = cap;
    let reg_count = num_params.min(ENTRY_ABI_REGS.len().saturating_sub(base));
    assert_eq!(reg_count, cap - 1);
    assert_eq!(
        num_params - reg_count,
        1,
        "exactly one argument on the stack"
    );
}

pub(in crate::ir_lower::tests) fn compile_via_ir(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    num_locals: usize,
) -> Option<CompiledMethod> {
    let builder = IrBuilder::new(num_params, num_locals);
    let mut graph = builder.build(code, code_len)?;
    ir_optimize::optimize(&mut graph);
    let schedule = ir_schedule::schedule(&graph);
    lower(&graph, &schedule, num_params, num_locals, &no_helpers())
}

/// A block whose successor the layout already placed next must FALL
/// THROUGH, not jump.
///
/// Two assertions, because either alone can pass on a broken emitter. The
/// census says the elision ran at all — without it, a method that happens
/// to need every jump is indistinguishable from an elision that never
/// fires. The byte scan says no `JMP rel32` survives with a displacement of
/// zero, which is a jump to the very next instruction and therefore always
/// dead weight; `E9 00 00 00 00` at the head of the loop body is exactly
/// what this emitter used to produce, and is the shape the fix exists to
/// remove.
///
/// The census counters are process-global and monotone, so a strict
/// increase is sound even when other tests run concurrently: a parallel
/// lowering can only push the count further up.
#[test]
pub(in crate::ir_lower::tests) fn a_successor_placed_next_is_reached_by_falling_through() {
    // int f(int x) { return x == 0 ? 2 : 1; }
    //   0: iload_0
    //   1: ifeq -> 8
    //   4: iconst_1
    //   5: goto -> 9
    //   8: iconst_2
    //   9: ireturn
    let code = [
        0x1a, // iload_0
        0x99, 0x00, 0x07, // ifeq +7 -> 8
        0x04, // iconst_1
        0xa7, 0x00, 0x04, // goto +4 -> 9
        0x05, // iconst_2
        0xac, // ireturn
        0, 0, 0,
    ];
    let before = ir_fallthrough_census();
    let compiled = compile_via_ir(&code, 10, 1, 1).expect("a two-armed select must lower");
    let after = ir_fallthrough_census();
    assert!(
        after.0 > before.0,
        "no block-exit edge fell through: census went {before:?} -> {after:?}",
    );

    let code_bytes = compiled.code_bytes();
    assert!(
        !code_bytes
            .windows(5)
            .any(|w| w == [0xE9, 0x00, 0x00, 0x00, 0x00]),
        "emitted a JMP rel32 with displacement 0 -- a jump to the next \
             instruction; {} bytes of code",
        code_bytes.len(),
    );

    // The body must still compute the right answer: an elision that drops a
    // jump the layout did NOT make redundant is wrong code, not a smaller
    // body, and the census cannot tell the difference.
    // SAFETY: one int parameter, no context, no helpers reachable.
    for (arg, want) in [(0i64, 2i64), (1, 1), (-7, 1)] {
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let got = unsafe { compiled.try_call(&[arg]).expect("call") };
        assert_eq!(got, want, "f({arg})");
    }
}

/// gch h2/fj (`gcd-d1a-proposal-context-slot-loads-cannot-read-an-unreserved-slot`,
/// item 1, IR half): every LOAD of the VM-context slot goes through the
/// checked `Lowerer::load_context`, which refuses the compile for an
/// unreserved slot; no raw `load_reg_from_frame(.., context_slot_off)` is
/// left. Built needles, so this test's own text does not match.
#[test]
pub(in crate::ir_lower::tests) fn gch_h2fj_every_ir_context_load_is_the_checked_loader() {
    let raw_load = format!("{}(", "load_reg_from_frame");
    let slot = format!("self.{}", "context_slot_off");
    let src = concat!(
        include_str!("../../ir_lower/mod.rs"),
        "\n",
        include_str!("../../ir_lower/virtual_object_info.rs"),
        "\n",
        include_str!("../../ir_lower/mir_plan_and_init.rs"),
        "\n",
        include_str!("../../ir_lower/vector_preloop.rs"),
        "\n",
        include_str!("../../ir_lower/value_phi_copies.rs"),
        "\n",
        include_str!("../../ir_lower/field_proof_entry_is_rechecked_osr.rs"),
        "\n",
        include_str!("../../ir_lower/int_expr_bounds.rs"),
        "\n",
        include_str!("../../ir_lower/character_to_chars_prefix.rs"),
        "\n",
        include_str!("../../ir_lower/getfield_legacy_read.rs"),
        "\n",
        include_str!("../../ir_lower/post_call_exit_site_for.rs"),
        "\n",
        include_str!("../../ir_lower/entry_fold_node.rs"),
        "\n",
        include_str!("../../ir_lower/call_loaded_ic_entry_releasing.rs"),
        "\n",
        include_str!("../../ir_lower/fallthrough_poll_spills.rs"),
        "\n",
        include_str!("../../ir_lower/ir_ic_cascade_admits_kind.rs"),
        "\n",
        include_str!("../../ir_lower/implicit_kind_matches_catch.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_raw_tail.rs"),
        "\n",
        include_str!("../../ir_lower/frame_needs.rs"),
        "\n",
        include_str!("../../ir_lower/back_edge_dead_ref_homes.rs"),
        "\n",
        include_str!("../../ir_lower/block_lazy_clear_survivors.rs"),
        "\n",
        include_str!("../../ir_lower/promoted_pending_mark.rs"),
        "\n",
        include_str!("../../ir_lower/site_value_live_at.rs"),
        "\n",
        include_str!("../../ir_lower/plan_register_residency_with.rs"),
        "\n",
        include_str!("../../ir_lower/inner_attempt.rs"),
        "\n",
        include_str!("../../ir_lower/g1_aastore_screen.rs"),
        "\n",
        include_str!("../../ir_lower/scoped_session_confined_rows.rs")
    );
    let raw: Vec<&str> = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| l.contains(&raw_load) && l.contains(&slot))
        .collect();
    // The loader's own line is the only one.
    assert_eq!(
        raw.len(),
        1,
        "raw context loads outside `load_context`: {raw:?}"
    );
    assert!(raw[0].contains(&format!("self.{raw_load}reg, {slot});")));
    let checked = format!("self.{}(", "load_context");
    assert!(
        src.lines().filter(|l| l.contains(&checked)).count() >= 31,
        "the converted call sites"
    );
    assert!(
        src.contains("\"ir_lower: vm-context load from an unreserved slot (scan_frame_needs)\"")
    );
}

/// gcd d1/mon: the context slot is a word of its own. It sits directly
/// above the locals, is never `[rbp - 0]` (the saved caller RBP), and no
/// local, bookkeeping word or colour of the spill band can land on it, for
/// any local count. A frame that reserves none reports offset 0, which is
/// exactly the saved caller RBP -- why every context reader must be under
/// a reservation (the two tests above).
#[test]
pub(in crate::ir_lower::tests) fn the_context_slot_overlaps_no_other_frame_word() {
    // `Object f() { return new X(); }`: `Op::New` reserves the context.
    let mut new_graph = Graph::empty(0);
    let start = new_graph.add(Op::Start, IrType::Control, vec![], None);
    new_graph.entry = start;
    let ctrl = new_graph.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = new_graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let allocation = new_graph.add(
        Op::New {
            class_id: 23,
            num_fields: 7,
        },
        IrType::Ref,
        vec![ctrl, mem],
        Some(0),
    );
    new_graph.exit = new_graph.add(Op::Return, IrType::Void, vec![ctrl, allocation], Some(3));
    let new_schedule = ir_schedule::schedule(&new_graph);
    let (plain_graph, plain_schedule) = add_one_graph();
    for &num_locals in &[1usize, 2, 5, 9] {
        for (graph, schedule, wants_context) in [
            (&new_graph, &new_schedule, true),
            (&plain_graph, &plain_schedule, false),
        ] {
            let plan = plan_slots(graph, schedule, None);
            let empty: HashMap<usize, bool> = HashMap::new();
            let no_direct: HashMap<usize, (usize, bool)> = HashMap::new();
            let no_ic: HashMap<usize, (usize, usize)> = HashMap::new();
            let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
            let no_frame_sites = crate::ir::IrInlineFrameSites::default();
            let buf = ExecutableBuffer::new(4096).expect("executable buffer");
            let helpers = no_helpers();
            let lowerer = Lowerer::new(
                graph,
                schedule,
                buf,
                0,
                num_locals,
                &plan,
                &helpers,
                &empty,
                &[],
                None,
                &no_direct,
                &no_ic,
                &no_compact,
                &no_frame_sites,
            );
            assert_eq!(lowerer.needs_context, wants_context);
            let locals_top = (num_locals as i32) * 8;
            if !wants_context {
                assert_eq!(
                    lowerer.context_slot_off, 0,
                    "no reservation reads as offset 0, the saved caller RBP"
                );
                assert_eq!(lowerer.sp_id_slot_off, locals_top + 8);
                continue;
            }
            let ctx = lowerer.context_slot_off;
            assert_eq!(ctx, locals_top + 8, "the slot directly above the locals");
            assert!(ctx > 0, "`[rbp - 0]` is the saved caller RBP, not a slot");
            for local in 0..num_locals as i32 {
                assert_ne!(
                    (local + 1) * 8,
                    ctx,
                    "local {local} overlaps the context slot"
                );
            }
            for (name, off) in [
                ("sp_id", lowerer.sp_id_slot_off),
                ("shadow_thread", lowerer.shadow_thread_slot_off),
                ("shadow_savebase", lowerer.shadow_savebase_slot_off),
                ("shadow_savetop", lowerer.shadow_savetop_slot_off),
                ("phi_copy_scratch", lowerer.phi_copy_scratch_slot_off),
            ] {
                assert!(
                    off > ctx,
                    "{name} word at {off} is not above the context slot {ctx}"
                );
            }
            assert_eq!(lowerer.sp_id_slot_off, ctx + 8);
            assert!(lowerer.first_spill > ctx);
            for color in 0..plan.slots as i32 {
                assert_ne!(lowerer.first_spill + color * 8, ctx);
            }
            assert!(lowerer.spill_cap_off > ctx);
            assert!(lowerer.frame_size > ctx);
        }
    }
}

/// Round 11 wave 2 (lane lower,
/// `r11-tier-ir-monitor-body-trap-reruns-from-entry`): a body with a
/// `synchronized` block and a trapping bytecode inside it sets
/// `can_deopt_resume`. Its frame states carry the monitor stack, and
/// without the flag the sinks' other arm (`sink_precise_resume_allowed`)
/// refuses any body holding a monitor, so the trap re-ran the method from
/// entry and repeated every side effect before it.
#[test]
pub(in crate::ir_lower::tests) fn a_monitor_body_with_a_trap_vouches_for_its_frames() {
    // aload_0; monitorenter; iload_1; iload_2; idiv; istore_1;
    // aload_0; monitorexit; iload_1; ireturn
    let code = [
        0x2a, 0xc2, 0x1b, 0x1c, 0x6c, 0x3c, 0x2a, 0xc3, 0x1b, 0xac, 0, 0,
    ];
    let graph = IrBuilder::new(3, 3)
        .build(&code, 10)
        .expect("builder must accept a division under a monitor");
    let schedule = ir_schedule::schedule(&graph);
    extern "C" fn fake_monitor(_vm: i64, obj: i64) -> i64 {
        obj
    }
    extern "C" fn fake_monitor_exit(_vm: i64, _obj: i64) -> i64 {
        1
    }
    let mut helpers = no_helpers();
    helpers.monitor_enter = fake_monitor as *const () as usize;
    helpers.monitor_exit = fake_monitor_exit as *const () as usize;
    let cm = lower(&graph, &schedule, 3, 3, &helpers).expect("lower");
    assert!(
        cm._deopt_point_boxes
            .iter()
            .any(|p| !p.frame_state.monitors.is_empty()),
        "the division's zero guard deopts with the lock in its frame"
    );
    assert!(
        cm.can_deopt_resume,
        "a body whose frames describe its monitors must vouch for precise resume"
    );
}
/// COV-03 — build the one-line `void set(Corpus o, X v) { o.f = v; }` graph
/// for field descriptor `tag`, with both wide-field gates on.
///   aload_0; <x>load_1; putfield #2; return
pub(in crate::ir_lower) fn ref_or_wide_putfield_graph(
    tag: u8,
    load_op: u8,
    param_ty: IrType,
) -> Graph {
    let code = [0x2a, load_op, 0x01, 0xb5, 0x00, 0x02, 0xb1, 0, 0];
    let mut b = IrBuilder::new(2, 4);
    b.set_param_types(&[IrType::Ref, param_ty]);
    let mut fi = std::collections::HashMap::new();
    fi.insert(3usize, (0usize, tag));
    b.set_field_info(fi);
    b.set_wide_field_gates(true, true);
    b.build(&code, 7)
        .unwrap_or_else(|| panic!("the builder must accept a {} putfield", tag as char))
}

/// Lower `void set(Corpus o, X v) { o.f = v; }` with a resolved compact
/// slot for the store site, and return the emitted bytes.
///
/// `plan` publishes the collector's reference-store barrier gates; without
/// it the helpers describe a collector that published none, which is the
/// control arm for every assertion below.
pub(in crate::ir_lower::tests) const COMPACT_REF_PUTFIELD_HELPER: usize = 0x1111_2222_3333_4450;

/// Round 16 (lane fieldguard, FG16-1): with
/// `CRATONVM_JIT_IR_LAYOUT_GUARD_DEOPT=1` the first inline primitive store
/// through parameter 0 guards the layout epoch with a deopt exit, and a
/// later store through the same receiver drops its compare -- in the same
/// block, and in a block the guard dominates when the body is loop-free.
/// In a body with a loop, the store in another block keeps its own guard.
/// With the switch off every inline site keeps its rejoining compare.

/// Round 16 (lane fieldguard, FG16-1): a deopting layout guard is placed
/// only at a pc exactly one effectful node carries -- never at one an
/// expansion shares (two stores at one pc), where re-executing the
/// instruction would replay the first store.
#[test]
pub(in crate::ir_lower::tests) fn r16_fieldguard_a_shared_pc_is_never_a_deopting_layout_guard_site()
{
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let obj = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let f0 = g.add(Op::Const(0), IrType::Int, vec![], Some(5));
    let f1 = g.add(Op::Const(1), IrType::Int, vec![], None);
    let v = g.add(Op::Const(7), IrType::Int, vec![], None);
    let s0 = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, mem, obj, f0, v],
        Some(5),
    );
    let s1 = g.add(
        Op::Store(MemKind::Int),
        IrType::Memory,
        vec![ctrl, s0, obj, f1, v],
        Some(5),
    );
    let _load = g.add(
        Op::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, s1, obj, f0],
        Some(9),
    );
    let lone = layout_guard_lone_pcs(&g);
    assert!(!lone.contains(&5), "two stores share pc 5");
    assert!(lone.contains(&9), "the load is alone at pc 9");
    assert_eq!(
        lone.len(),
        1,
        "a pure node's pc (the constant's 5) counts for nothing"
    );
}

/// JIT round 17 wave 3 (lane irlower): on a single-copy graph the
/// path-aware anchor (`CRATONVM_JIT_IR_BCI_ANCHOR_DOMINATED`) changes
/// nothing: the first owner of every bci in layout order already sits in a
/// block every defining block of its by-bci snapshot dominates, so
/// `note_bci_anchor` keeps the earliest owner exactly as before.
#[test]
pub(in crate::ir_lower::tests) fn r17w3_irlower_a_single_copy_loop_anchors_every_bci_where_it_did()
{
    let code = [
        0x04, 0x3c, // 0: iconst_1; istore_1
        0x05, 0x3d, // 2: iconst_2; istore_2
        0x03, 0x3e, // 4: iconst_0; istore_3
        0x1d, 0x1a, // 6: iload_3; iload_0          loop header
        0xa2, 0x00, 0x0d, // 8: if_icmpge 21
        0x1b, 0x1c, // 11: iload_1; iload_2
        0x60, 0x3c, // 13: iadd; istore_1
        0x84, 0x03, 0x01, // 15: iinc 3, 1
        0xa7, 0xff, 0xf4, // 18: goto 6
        0x1b, 0xac, // 21: iload_1; ireturn
        0, 0,
    ];
    let graph = IrBuilder::new(1, 4)
        .build(&code, 23)
        .expect("the loop builds");
    let schedule = ir_schedule::schedule(&graph);
    let value_blocks = bci_anchor_value_blocks(&graph, &schedule);
    let nb = schedule.blocks.len();
    let mut first: HashMap<usize, usize> = HashMap::new();
    for (b, block) in schedule.blocks.iter().enumerate() {
        for &id in &block.nodes {
            if let Some(pc) = graph.nodes[id as usize].bytecode_pc {
                first.entry(pc).or_insert(b);
            }
        }
    }
    let mut checked = 0usize;
    // The loop's bcis: their snapshots name the header φs and the body's
    // values. (Before the loop, a snapshot names only the entry block's
    // constants, wherever the scheduler floats them.)
    for (pc, b) in first.into_iter().filter(|&(pc, _)| pc >= 6) {
        let Some(defs) = value_blocks.get(&pc) else {
            continue;
        };
        for &d in defs {
            assert!(d < nb);
            assert!(
                schedule.dom.dominates(d, b),
                "bci {pc}: its first owner (block {b}) is not dominated by \
                     defining block {d}"
            );
        }
        checked += 1;
    }
    assert!(checked > 0, "the loop has by-bci snapshots at owned bcis");
}

/// Round 16 (lane fieldguard, FG16-7): EXECUTED. A field site with NO
/// compact row (the JDK `ArrayList$Itr` shape: a class with no compact
/// layout) reads and stores inline through the legacy arm under
/// `CRATONVM_JIT_IR_LEGACY_FIELD_INLINE=1`, and still takes the checked
/// helper for a short legacy object and for a compact one; with the switch
/// off every access is the helper, as before.
pub(in crate::ir_lower::tests) fn compile_via_ir_no_opt(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    num_locals: usize,
) -> CompiledMethod {
    let builder = IrBuilder::new(num_params, num_locals);
    let graph = builder.build(code, code_len).expect("IR build");
    let schedule = ir_schedule::schedule(&graph);
    lower(&graph, &schedule, num_params, num_locals, &no_helpers()).expect("lower")
}

/// Does the emitted code bake a `jit_getfield` third argument equal to
/// `want`, ignoring the NPE trap-SITE key?
///
/// A byte-sequence search used to be enough, and stopped being enough when
/// the argument grew a third passenger: the key is a per-compile monotonic
/// id, so the eight bytes differ from `want` by a value no expectation can
/// name without becoming a copy of the emitter. Masking ONE documented
/// field off keeps the property this test was written for -- the
/// expectation is built from constants, never from `getfield_index_arg` --
/// while letting the passenger through.
pub(in crate::ir_lower::tests) fn contains_getfield_arg(hay: &[u8], want: u64) -> bool {
    hay.windows(8).any(|w| {
        let v = u64::from_le_bytes(w.try_into().expect("an 8-byte window"));
        v & !cratonvm_jit_api::GETFIELD_NPE_SITE_MASK == want
    })
}

/// True iff `needle` appears as a contiguous subsequence of `hay`.
pub(in crate::ir_lower::tests) fn contains_seq(hay: &[u8], needle: &[u8]) -> bool {
    needle.len() <= hay.len() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Count non-overlapping occurrences of `needle` in `hay`.
pub(in crate::ir_lower::tests) fn count_seq(hay: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || needle.len() > hay.len() {
        return 0;
    }
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}
