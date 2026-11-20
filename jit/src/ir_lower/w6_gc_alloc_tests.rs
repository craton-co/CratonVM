// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use super::*;

fn test_lowerer(buf_cap: usize) -> Lowerer<'static> {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let arr = g.add(Op::Param(0), IrType::Ref, vec![start], None);
    let idx = g.add(Op::Const(1), IrType::Int, vec![], None);
    let val = g.add(Op::Param(1), IrType::Ref, vec![start], None);
    g.exit = g.add(Op::Return, IrType::Void, vec![c0], Some(0));
    let _ = (arr, idx, val);
    let graph: &'static Graph = Box::leak(Box::new(g));
    let schedule: &'static Schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let empty: &'static HashMap<usize, bool> = Box::leak(Box::new(HashMap::new()));
    let no_direct: &'static HashMap<usize, (usize, bool)> = Box::leak(Box::new(HashMap::new()));
    let no_ic: &'static HashMap<usize, (usize, usize)> = Box::leak(Box::new(HashMap::new()));
    let no_compact: &'static HashMap<(usize, bool), (u32, bool, u8)> =
        Box::leak(Box::new(HashMap::new()));
    let no_frame_sites: &'static crate::ir::IrInlineFrameSites =
        Box::leak(Box::new(crate::ir::IrInlineFrameSites::default()));
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` of word-sized fields.
    let helpers: &'static JitRuntimeHelpers = Box::leak(Box::new(unsafe { std::mem::zeroed() }));
    let buf = ExecutableBuffer::new(buf_cap).expect("executable buffer");
    Lowerer::new(
        graph,
        schedule,
        buf,
        0,
        2,
        plan,
        helpers,
        empty,
        &[],
        None,
        no_direct,
        no_ic,
        no_compact,
        no_frame_sites,
    )
}

#[test]
fn gated_aastore_serial_checks_pre_once_and_uses_rip_relative_when_in_reach() {
    let mut lo = test_lowerer(4096);
    let base = lo.buf.as_ptr() as usize;
    // Out-of-reach gate addresses check single emission of `pre` via `MOV R11, imm64`.
    let pre_far = base.wrapping_add(0x1_0000_0000);
    let post_far = pre_far + 8;
    let floor_far = pre_far + 16;
    let view = Box::leak(Box::new([0usize; 32])).as_ptr() as usize;
    lo.ref_store_gates = Some((pre_far, post_far, floor_far));
    lo.ref_store_post_skip_mask = None;
    lo.zgc_fused_gate = None;
    lo.ir_card_view = Some((view, true));
    let arr = 2;
    let idx = 3;
    let val = 4;
    let before = lo.buf.pos();
    let _ = lo.emit_gated_ir_aastore((pre_far, post_far, floor_far), arr, idx, val, false);
    let bytes = &lo.buf.as_slice()[before..];
    let mut mov_pre = vec![0x49, 0xBB];
    mov_pre.extend_from_slice(&(pre_far as u64).to_le_bytes());
    let pre_count = bytes
        .windows(mov_pre.len())
        .filter(|w| *w == mov_pre.as_slice())
        .count();
    assert_eq!(
        pre_count, 1,
        "Serial gated aastore must test `pre` only once"
    );

    // In-reach addresses emit RIP-relative `CMP byte [RIP+disp32], 0` and `CMP CL, [RIP+disp32]`.
    let b2 = lo.buf.pos();
    let near = base + b2 + 0x100;
    lo.emit_ir_gate_byte_zero_cmp(near);
    let gbytes = &lo.buf.as_slice()[b2..];
    assert_eq!(&gbytes[..2], &[0x80, 0x3D]);
    assert_eq!(gbytes.len(), 7);

    let b3 = lo.buf.pos();
    let near_floor = base + b3 + 0x100;
    lo.emit_ir_cmp_cl_mem_byte(near_floor);
    let fbytes = &lo.buf.as_slice()[b3..];
    assert_eq!(&fbytes[..2], &[0x3A, 0x0D]);
    assert_eq!(fbytes.len(), 6);
}

#[test]
fn alloc_prefetch_skipped_for_small_const_allocations() {
    let has_prefetch = |b: &[u8]| {
        b.windows(3)
            .any(|w| w == [0x0F, 0x0D, 0x88] || w == [0x0F, 0x18, 0x80])
    };
    let obj_plan = |num_fields: usize| crate::runtime_lowering::InlineTlabPlan {
        thread_slot_off: 8,
        thread_tls_disp: 0,
        cursor_off: 16,
        end_off: 24,
        post_init: 0x1000,
        context_off: 16,
        class_id: 7,
        num_fields,
        announce: 0,
        skip_post_init: true,
        register_first: 0,
        post_init_helper_first: false,
    };
    let mut small_obj = ExecutableBuffer::new(1024).unwrap();
    assert!(crate::runtime_lowering::emit_inline_tlab_new_ir(
        &mut small_obj,
        &obj_plan(1),
        0x2000,
        0
    )
    .is_some());
    assert!(
        !has_prefetch(small_obj.as_slice()),
        "total <= 64 skips prefetch"
    );

    let mut big_obj = ExecutableBuffer::new(1024).unwrap();
    assert!(crate::runtime_lowering::emit_inline_tlab_new_ir(
        &mut big_obj,
        &obj_plan(10),
        0x2000,
        0
    )
    .is_some());
    assert!(
        !crate::runtime_lowering::ir_alloc_prefetch_enabled() || has_prefetch(big_obj.as_slice()),
        "total > 64 keeps prefetch"
    );

    let arr_plan = |const_length: Option<u32>| crate::runtime_lowering::InlineTlabArrayPlan {
        thread_slot_off: 8,
        thread_tls_disp: 0,
        cursor_off: 16,
        end_off: 24,
        context_off: 16,
        atype: 10, // T_INT
        length_off: 32,
        announce: 0,
        component_class_id: None,
        const_length,
    };
    let mut small_arr = ExecutableBuffer::new(1024).unwrap();
    assert!(crate::runtime_lowering::emit_inline_tlab_newarray_ir(
        &mut small_arr,
        &arr_plan(Some(4)),
        0x2000,
        0
    )
    .is_some());
    assert!(
        !has_prefetch(small_arr.as_slice()),
        "new int[4] (<= 64 bytes) skips prefetch"
    );

    let mut big_arr = ExecutableBuffer::new(1024).unwrap();
    assert!(crate::runtime_lowering::emit_inline_tlab_newarray_ir(
        &mut big_arr,
        &arr_plan(Some(32)),
        0x2000,
        0
    )
    .is_some());
    assert!(
        !crate::runtime_lowering::ir_alloc_prefetch_enabled() || has_prefetch(big_arr.as_slice()),
        "new int[32] (> 64 bytes) keeps prefetch"
    );
}

/// w7: the reference `aastore`'s index is loaded with the 32-bit `MOV ECX,
/// ...` of `gp_load_index_rcx`, which zero-extends, so no `MOV ECX, ECX`
/// (`89 C9`) precedes any element store of the gated arm.
#[test]
fn gated_aastore_index_load_needs_no_mov_ecx_ecx() {
    let mut lo = test_lowerer(4096);
    let base = lo.buf.as_ptr() as usize;
    let pre = base.wrapping_add(0x1_0000_0000);
    let view = Box::leak(Box::new([0usize; 32])).as_ptr() as usize;
    lo.ref_store_gates = Some((pre, pre + 8, pre + 16));
    lo.ref_store_post_skip_mask = Some(cratonvm_types::GC_FLAG_OLD_GEN);
    lo.zgc_fused_gate = None;
    for precise in [false, true] {
        lo.ir_card_view = Some((view, precise));
        let before = lo.buf.pos();
        let _ = lo.emit_gated_ir_aastore((pre, pre + 8, pre + 16), 2, 3, 4, false);
        let bytes = &lo.buf.as_slice()[before..];
        let stores: Vec<usize> = bytes
            .windows(4)
            .enumerate()
            .filter(|(_, w)| *w == [0x48, 0x89, 0x54, 0xC8] || *w == [0x48, 0x89, 0x94, 0xC8])
            .map(|(i, _)| i)
            .collect();
        assert!(!stores.is_empty(), "the gated arm stores inline");
        for at in stores {
            assert!(
                at < 2 || bytes[at - 2..at] != [0x89, 0xC9],
                "no MOV ECX, ECX before the element store at +{at} (precise={precise})"
            );
        }
    }
}

/// w7: the optimizing tier's inline ZGC `new` uses the TLAB cursor as loaded
/// -- `MOV R11, [R10 + cursor]` is followed directly by the bump's `LEA RAX,
/// [R11 + size]` -- since a ZGC cursor is 8-aligned by construction
/// (`runtime_lowering::zgc_tlab_align_test_enabled`); Generational and G1
/// (`announce == 0`) keep `TEST R11B, 7; JNE .slow`.
#[test]
fn zgc_inline_tlab_new_has_no_cursor_alignment_test() {
    let plan = |announce: usize| crate::runtime_lowering::InlineTlabPlan {
        thread_slot_off: 8,
        thread_tls_disp: 0,
        cursor_off: 16,
        end_off: 24,
        post_init: 0x1000,
        context_off: 16,
        class_id: 7,
        num_fields: 2,
        announce,
        skip_post_init: true,
        register_first: 0,
        post_init_helper_first: false,
    };
    let after_cursor_load = |announce: usize| {
        let mut buf = ExecutableBuffer::new(1024).unwrap();
        assert!(crate::runtime_lowering::emit_inline_tlab_new_ir(
            &mut buf,
            &plan(announce),
            0x2000,
            0
        )
        .is_some());
        let code = buf.as_slice().to_vec();
        let load = [0x4D, 0x8B, 0x9A, 16, 0, 0, 0]; // MOV R11, [R10 + 16]
        let at = code
            .windows(load.len())
            .position(|w| w == load)
            .expect("the cursor load");
        code[at + load.len()..at + load.len() + 4].to_vec()
    };
    assert_eq!(
        &after_cursor_load(0x3000)[..3],
        &[0x49, 0x8D, 0x83],
        "ZGC: LEA RAX, [R11 + disp32] right after the load"
    );
    if crate::runtime_lowering::ir_tlab_align_test_any_gc_enabled() {
        assert_eq!(
            after_cursor_load(0),
            vec![0x41, 0xF6, 0xC3, 0x07],
            "Generational / G1: TEST R11B, 7"
        );
    }
}
