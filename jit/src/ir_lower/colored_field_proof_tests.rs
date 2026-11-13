// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Receiver proofs and the colored field barrier exercised together.

use super::*;
use crate::ir::IrBuilder;
use crate::x64::{ZgcColoredFieldLoadTestOverride, zgc_read_gate_flip_test_lock};
use cratonvm_types::narrow_oop::{
    COLORED_SLOT_METADATA_MASK, COLORED_SLOT_REMAPPED, COLORED_SLOT_TAG,
};

#[repr(C)]
struct HelperState {
    calls: usize,
    reply: i64,
}

unsafe extern "C" fn checked_field(vm: i64, _object: i64, _index: i64) -> i64 {
    // SAFETY: the fixture passes its live HelperState as the VM context.
    let state = unsafe { &mut *(vm as *mut HelperState) };
    state.calls += 1;
    state.reply
}

fn compile(
    on: bool,
    registry: Option<std::collections::HashSet<usize>>,
    bounds: &[u64; 6],
) -> CompiledMethod {
    cratonvm_types::flags::with_thread_overrides(
        &[
            (
                "CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS",
                Some(if on { "1" } else { "0" }),
            ),
            ("CRATONVM_JIT_IR_OPERAND_IN_PLACE", Some("0")),
            ("CRATONVM_JIT_IR_LAYOUT_GUARD_DEOPT", Some("0")),
        ],
        || {
            // return o.i == 40 ? o.ref : null;
            let code = [
                0x2a, 0xb4, 0x00, 0x02, 0x10, 0x28, 0xa0, 0x00, 0x08, 0x2a, 0xb4, 0x00, 0x03, 0xb0,
                0x01, 0xb0,
            ];
            let mut builder = IrBuilder::new(1, 1);
            builder.set_param_types(&[IrType::Ref]);
            builder.set_field_info([(1, (0, b'I')), (10, (1, b'L'))].into_iter().collect());
            let mut graph = builder
                .build(&code, code.len())
                .expect("two-field body builds");
            graph.proven_compact_withdrawn_pcs = registry;
            let schedule = crate::ir_schedule::schedule(&graph);
            // SAFETY: the helper table contains integer fields. Only getfield
            // and read-bounds metadata are needed by this call-free fixture.
            let mut helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            helpers.getfield = checked_field as *const () as usize;
            helpers.read_bounds_addr = bounds.as_ptr() as usize;
            let fields = [
                ((1, false), (8, false, b'I')),
                ((10, true), (16, true, b'L')),
            ]
            .into_iter()
            .collect();
            lower_inner(
                &graph,
                &schedule,
                1,
                1,
                &helpers,
                &HashMap::new(),
                &[],
                None,
                &HashMap::new(),
                &HashMap::new(),
                &fields,
                &crate::ir::IrInlineFrameSites::default(),
            )
            .expect("colored two-field body lowers")
        },
    )
}

fn compact_tests(code: &[u8]) -> usize {
    let mut test = vec![0xF6, 0x80];
    test.extend_from_slice(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
    test.push(cratonvm_types::GC_FLAG_COMPACT);
    code.windows(test.len())
        .filter(|bytes| *bytes == test.as_slice())
        .count()
}

#[test]
fn colored_reads_reuse_receiver_proofs_and_keep_the_barrier() {
    let _lock = zgc_read_gate_flip_test_lock();
    if cratonvm_types::narrow_oop::narrow_oops_enabled()
        || crate::x64::inline_getfield_enabled()
        || !crate::x64::guarded_inline_getfield_enabled()
        || !crate::x64::trusted_oop_receiver_getfield_enabled()
    {
        return;
    }
    const BASE: u64 = 0x0000_7F00_0000_0000;
    const MARKED: u64 = 1 << 42;
    let mut bad_mask = Box::new(COLORED_SLOT_METADATA_MASK ^ COLORED_SLOT_REMAPPED);
    let bounds = Box::new([0u64; 6]);
    let mut comparisons = 0;
    let mut counts = Vec::new();
    let mut legacy_outcomes = Vec::new();
    for flip in [false, true] {
        let cell = flip.then_some((&*bad_mask as *const u64) as u64);
        let _colored = ZgcColoredFieldLoadTestOverride::open(BASE, cell);
        for (on, registry, expected_tests, legacy_deopts) in [
            (false, Some(std::collections::HashSet::new()), 2, false),
            (true, Some(std::collections::HashSet::new()), 1, true),
            (true, None, 2, false),
            (true, Some([1].into_iter().collect()), 2, true),
        ] {
            let mut object = Box::new([0u64; 8]);
            // SAFETY: both fields and the header flag are inside the allocation.
            unsafe {
                let bytes = object.as_mut_ptr().cast::<u8>();
                *bytes.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = cratonvm_types::GC_FLAG_COMPACT;
                *bytes.add(8).cast::<i32>() = 40;
            }
            let default_mask = COLORED_SLOT_METADATA_MASK ^ COLORED_SLOT_REMAPPED;
            for (word, mask) in [
                (0, default_mask),
                (0x1234, default_mask),
                (
                    COLORED_SLOT_TAG | COLORED_SLOT_REMAPPED | 0x280,
                    default_mask,
                ),
                (COLORED_SLOT_TAG | MARKED | 0x280, default_mask),
                (
                    COLORED_SLOT_TAG | COLORED_SLOT_REMAPPED | 0x280,
                    COLORED_SLOT_METADATA_MASK ^ MARKED,
                ),
            ] {
                *bad_mask = mask;
                object[2] = word;
                let mut state = HelperState {
                    calls: 0,
                    reply: 0x5150,
                };
                let cm = compile(on, registry.clone(), &bounds);
                counts.push((compact_tests(cm.code_bytes()), expected_tests));
                let before = *object;
                // SAFETY: native code reads this live shaped object. A bad
                // colored word uses checked_field and the supplied live context.
                let answer = unsafe {
                    cm.call_with_heap(
                        (&mut state as *mut HelperState) as i64,
                        &[object.as_ptr() as i64],
                    )
                };
                let bad = flip && word & COLORED_SLOT_TAG != 0 && word & mask != 0;
                let expected = if bad {
                    state.reply
                } else if word & COLORED_SLOT_TAG != 0 {
                    (BASE + (word & cratonvm_types::narrow_oop::COLORED_SLOT_OFFSET_MASK)) as i64
                } else {
                    word as i64
                };
                assert_eq!(
                    answer, expected,
                    "the barrier returns an exact decoded reference"
                );
                assert_eq!(*object, before, "field reads never alter the object");
                assert_eq!(
                    state.calls,
                    usize::from(flip && word & COLORED_SLOT_TAG != 0 && word & mask != 0),
                    "a bad color reaches the checked helper"
                );
                comparisons += 1;
            }

            // A real legacy shape must retain dispatch or leave at the
            // establishing guard. Each path reads two legacy Value cells.
            object.fill(0);
            unsafe {
                let bytes = object.as_mut_ptr().cast::<u8>();
                *bytes.add(cratonvm_types::NUM_SLOTS_OFFSET).cast::<u32>() = 2;
                *bytes
                    .add(cratonvm_types::HEADER_SIZE + cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET)
                    .cast::<i32>() = 40;
                *bytes.add(cratonvm_types::HEADER_SIZE + 16).cast::<u32>() =
                    cratonvm_types::FIELD_CELL_TAG_OBJECT;
                *bytes
                    .add(
                        cratonvm_types::HEADER_SIZE
                            + 16
                            + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET,
                    )
                    .cast::<u64>() = 0x1234;
            }
            let cm = compile(on, registry.clone(), &bounds);
            let mut state = HelperState {
                calls: 0,
                reply: 0x5150,
            };
            let _ = crate::deopt::take_last_deopt();
            // SAFETY: the object carries complete legacy cells for both fields.
            let answer = unsafe {
                cm.call_with_heap(
                    (&mut state as *mut HelperState) as i64,
                    &[object.as_ptr() as i64],
                )
            };
            let deopt = crate::deopt::take_last_deopt();
            if answer == i64::MIN {
                assert!(deopt.is_some(), "a legacy guard publishes its frame");
            } else {
                assert_eq!(answer, 0x1234, "the legacy dispatch returns the reference");
                assert!(deopt.is_none());
            }
            legacy_outcomes.push((answer == i64::MIN, legacy_deopts));
            assert_eq!(state.calls, 0);
            comparisons += 1;
        }
    }
    eprintln!("NATIVE-COLORED-FIELD-PROOFS {comparisons}");
    for (actual, expected) in counts {
        assert_eq!(
            actual, expected,
            "colored read reuses the first field's receiver proof"
        );
    }
    for (deopted, expected) in legacy_outcomes {
        assert_eq!(
            deopted, expected,
            "the admitting guard handles legacy receivers"
        );
    }
}
