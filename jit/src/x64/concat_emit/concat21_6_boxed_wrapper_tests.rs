use super::*;
use crate::concat_inline_plan::JitConcatInlinePlan;
use crate::x64::{R12, R13};
const A0: u8 = 7; // RDI
const A1: u8 = 6; // RSI

fn fake_wrapper_obj(
    class_id: u32,
    compact: bool,
    off: usize,
    bytes: &[u8],
) -> Vec<u8> {
    let mut obj = vec![0u8; off + 16];
    let cid_off = 0;
    obj[cid_off..cid_off + 4].copy_from_slice(&class_id.to_le_bytes());
    if compact {
        obj[cratonvm_types::GC_FLAGS_BYTE_OFFSET] |= cratonvm_types::GC_FLAG_COMPACT;
    }
    obj[off..off + bytes.len()].copy_from_slice(bytes);
    obj
}

#[test]
fn concat_ir_site_declines_without_boxed_layouts() {
    let recipe: Vec<u16> = "v=\u{1}".encode_utf16().collect();
    let plan = Box::leak(Box::new(
        JitConcatInlinePlan::from_recipe(
            &recipe,
            &[],
            "(Ljava/lang/Integer;)Ljava/lang/String;",
        )
        .expect("plan"),
    ));
    let layout = crate::StringFieldLayout::new(0, Some(1), 2, 10);
    let frame = ConcatIrFrame {
        tls_disp: 0,
        thread_slot_off: 8,
        cursor_off: 8,
        end_off: 16,
        class_id_off: 0 as i32,
        g1_table: 0,
        region_bounds_addr: 0,
    };
    let why = concat_ir_site(
        Some(plan),
        Some(layout),
        Some(1),
        vec![8],
        frame,
        ConcatBoxedLayouts::default(),
    )
    .expect_err("declined");
    assert!(why.contains("missing boxed wrapper layout"), "{why}");
}

#[test]
fn boxed_emission_rejects_missing_layouts_and_invalid_kinds() {
    let recipe: Vec<u16> = "v=\u{1}".encode_utf16().collect();
    let plan = Box::leak(Box::new(
        JitConcatInlinePlan::from_recipe(&recipe, &[], "(Ljava/lang/Integer;)Ljava/lang/String;")
            .expect("plan"),
    ));
    let shape = ConcatPassShape {
        plan,
        outer: super::r19_concat5_shared_emit_tests::layout(),
        compact_flag_addr: 1,
        arg_offsets: vec![8],
        class_id_off: 0,
        len_d8: 0,
        data_d8: 0,
        max_count: 4096,
        boxed_int: None,
        boxed_layouts: ConcatBoxedLayouts::default(),
    };
    let mut wide_shape = shape.clone();
    let wide_recipe: Vec<u16> = "Ω=\u{1}".encode_utf16().collect();
    wide_shape.plan = Box::leak(Box::new(
        JitConcatInlinePlan::from_recipe(
            &wide_recipe,
            &[],
            "(Ljava/lang/Integer;)Ljava/lang/String;",
        )
        .expect("wide plan"),
    ));
    for pass in 0..4 {
        let mut buf = ExecutableBuffer::new(4096).expect("buffer");
        match pass {
            0 => emit_length_pass_latin1(&mut buf, &shape, &mut Vec::new(), None),
            1 => emit_fill_pass(&mut buf, &shape),
            2 => emit_length_pass_wide(&mut buf, &wide_shape, &mut Vec::new()),
            _ => emit_fill_pass_wide(&mut buf, &wide_shape),
        }
        assert_eq!(
            buf.codegen_failure_reason(),
            Some("concat-missing-boxed-layout")
        );
        assert!(buf.overflowed());
    }
    let mut buf = ExecutableBuffer::new(4096).expect("buffer");
    emit_load_boxed_payload(&mut buf, 0, ConcatArgKind::String);
    assert_eq!(
        buf.codegen_failure_reason(),
        Some("concat-invalid-boxed-kind")
    );
}

#[test]
fn boxed_wrappers_len_fill_null_and_promote() {
    let recipe: Vec<u16> = "[\u{1},\u{1},\u{1},\u{1}]".encode_utf16().collect();
    let plan = Box::leak(Box::new(
        JitConcatInlinePlan::from_recipe(
            &recipe,
            &[],
            "(Ljava/lang/Integer;Ljava/lang/Long;Ljava/lang/Character;Ljava/lang/Boolean;)Ljava/lang/String;",
        )
        .expect("plan"),
    ));
    let outer = super::r19_concat5_shared_emit_tests::layout();
    let compact_flag: u8 = 1;
    let layouts = ConcatBoxedLayouts {
        int: Some(ConcatBoxedInt { class_id: 21, compact_off: 16, legacy_off: 20 }),
        long: Some(ConcatBoxedInt { class_id: 22, compact_off: 16, legacy_off: 24 }),
        char: Some(ConcatBoxedInt { class_id: 23, compact_off: 16, legacy_off: 20 }),
        bool: Some(ConcatBoxedInt { class_id: 24, compact_off: 16, legacy_off: 20 }),
    };
    let shape = ConcatPassShape {
        plan,
        outer,
        compact_flag_addr: &compact_flag as *const u8 as usize,
        arg_offsets: vec![8, 16, 24, 32],
        class_id_off: 0,
        len_d8: u8::try_from(cratonvm_types::ARRAY_LENGTH_OFFSET).expect("disp8"),
        data_d8: u8::try_from(cratonvm_types::ARRAY_DATA_OFFSET).expect("disp8"),
        max_count: 4096,
        boxed_int: None,
        boxed_layouts: layouts,
    };

    let int_obj = fake_wrapper_obj(21, true, 16, &(-42i32).to_le_bytes());
    let long_obj = fake_wrapper_obj(22, false, 24, &9876543210i64.to_le_bytes());
    let char_latin = fake_wrapper_obj(23, true, 16, &(b'Z' as u16).to_le_bytes());
    let char_wide = fake_wrapper_obj(23, true, 16, &(0x03A9u16).to_le_bytes()); // Omega
    let bool_obj = fake_wrapper_obj(24, false, 20, &1i32.to_le_bytes());

    let compile = |wide: bool, s: &ConcatPassShape| -> ExecutableBuffer {
        let mut buf = ExecutableBuffer::new(1 << 14).expect("buf");
        buf.emit(&[0x55, 0x53, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57]);
        mov_r64_r64(&mut buf, 5, A0); // MOV RBP, A0
        mov_r64_r64(&mut buf, R12, A1); // MOV R12, A1 (dst array)
        let mut decline = Vec::new();
        let mut promote = Vec::new();
        if wide {
            emit_length_pass_wide(&mut buf, s, &mut decline);
        } else {
            emit_length_pass_promoting(&mut buf, s, &mut decline, &mut promote);
        }
        mov_r64_r64(&mut buf, R13, RCX); // Save char length in R13
        mov_r64_r64(&mut buf, R11, R12); // emit_fill_pass writes to [R11 + ARRAY_DATA_OFFSET]
        if wide {
            emit_fill_pass_wide(&mut buf, s);
        } else {
            emit_fill_pass(&mut buf, s);
        }
        mov_r64_r64(&mut buf, RAX, R13);
        let done = jmp_patch(&mut buf);
        for p in decline {
            patch_here(&mut buf, p);
        }
        mov_imm32_sx(&mut buf, RAX, -1);
        let done2 = jmp_patch(&mut buf);
        for p in promote {
            patch_here(&mut buf, p);
        }
        mov_imm32_sx(&mut buf, RAX, -2);
        patch_here(&mut buf, done);
        patch_here(&mut buf, done2);
        buf.emit(&[0x41, 0x5F, 0x41, 0x5E, 0x41, 0x5D, 0x41, 0x5C, 0x5B, 0x5D, 0xC3]);
        assert!(!buf.overflowed());
        crate::platform::make_executable(buf.as_ptr() as *mut u8, buf.capacity())
            .expect("exec");
        buf
    };

    let latin_buf = compile(false, &shape);
    let wide_plan = Box::leak(Box::new(plan.promotion_twin().expect("twin").clone()));
    let mut wide_shape = shape.clone();
    wide_shape.plan = wide_plan;
    let wide_buf = compile(true, &wide_shape);

    // SAFETY: compile made this buffer executable with this C ABI.
    let f_latin: extern "C" fn(u64, u64) -> i64 =
        unsafe { std::mem::transmute(latin_buf.as_ptr()) };
    // SAFETY: compile made this buffer executable with this C ABI.
    let f_wide: extern "C" fn(u64, u64) -> i64 = unsafe { std::mem::transmute(wide_buf.as_ptr()) };

    let mut stk = [0u64; 8];
    stk[7] = int_obj.as_ptr() as u64;
    stk[6] = long_obj.as_ptr() as u64;
    stk[5] = char_latin.as_ptr() as u64;
    stk[4] = bool_obj.as_ptr() as u64;
    // SAFETY: the pointer is one past this eight-element array.
    let rbp = unsafe { stk.as_ptr().add(8) } as u64;
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let mut out = vec![0u64; 32];
    let out_ptr = out.as_mut_ptr() as u64;
    let payload = |o: &[u64]| -> &[u8] {
        assert!(data + 128 <= std::mem::size_of_val(o));
        // SAFETY: the checked offset and length fit in the borrowed output buffer.
        unsafe { std::slice::from_raw_parts((o.as_ptr() as *const u8).add(data), 128) }
    };

    let n = f_latin(rbp, out_ptr);
    assert_eq!(n, 23);
    assert_eq!(&payload(&out)[..23], b"[-42,9876543210,Z,true]");

    stk[7] = 0;
    stk[6] = 0;
    stk[5] = 0;
    stk[4] = 0;
    let n = f_latin(rbp, out_ptr);
    assert_eq!(n, 21);
    assert_eq!(&payload(&out)[..21], b"[null,null,null,null]");

    stk[7] = int_obj.as_ptr() as u64;
    stk[6] = long_obj.as_ptr() as u64;
    stk[5] = char_wide.as_ptr() as u64;
    stk[4] = bool_obj.as_ptr() as u64;
    assert_eq!(f_latin(rbp, out_ptr), -2);
    let n_w = f_wide(rbp, out_ptr);
    assert_eq!(n_w, 46);
    let u16s: Vec<u16> = payload(&out)[..46]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    assert_eq!(String::from_utf16(&u16s).unwrap(), "[-42,9876543210,\u{03A9},true]");

    let bad_obj = fake_wrapper_obj(999, true, 16, &0i32.to_le_bytes());
    stk[7] = bad_obj.as_ptr() as u64;
    assert_eq!(f_latin(rbp, out_ptr), -1);
}
