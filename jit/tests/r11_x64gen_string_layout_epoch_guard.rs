// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 11 wave 3 (lane x64gen): the single-pass String-family intrinsics
//! bake compact `StringFieldLayout` offsets, so each must guard the process
//! layout-replacement epoch like every other baked-offset site
//! (`r11w2-x64gen-string-intrinsics-bake-compact-offsets-without-an-epoch-guard`).
//!
//! Byte census only: this does not bump the epoch, which is process-wide and
//! would race every sibling test that compiled an inline compact arm.

use cratonvm_jit::x64::compile;
use cratonvm_jit::{
    try_resolve_string_intrinsic, CompiledMethod, JitDirectCall, StringFieldLayout,
};
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r11_x64gen_string_layout_epoch_guard invoked a runtime helper");
    }
    let s = stub as *const () as usize;
    JitRuntimeHelpers {
        newarray: s,
        new_object: s,
        anewarray_object: s,
        baload: s,
        bastore: s,
        iaload: s,
        iastore: s,
        aaload: s,
        aastore: s,
        multianewarray_2d: s,
        arraylength: s,
        getfield: s,
        putfield_int: s,
        putfield_long: s,
        putfield_float: s,
        putfield_double: s,
        putfield_object: s,
        getstatic: s,
        putstatic_int: s,
        putstatic_long: s,
        putstatic_float: s,
        putstatic_double: s,
        putstatic_object: s,
        checkcast: s,
        instanceof_check: s,
        throw_aioobe: s,
        throw_arithmetic: s,
        invoke_dispatch: s,
        invoke_virtual_mic: s,
        lambda_int_to_double: s,
        write_barrier: s,
        satb_pre_write_barrier: s,
        uncommon_trap: s,
        math_fma_double: s,
        math_fma_float: s,
        tlab_end_offset_in_thread: 8,
        throw_exception: s,
        jit_npe_with_action: s,
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        ldc_string: s,
        set_throw_bci: s,
        service_callee_deopt: s,
        ..Default::default()
    }
}

const STRING_CLASS_ID: u32 = 0x5712_3400;

fn string_layout() -> StringFieldLayout {
    StringFieldLayout::new(0, Some(1), 2, STRING_CLASS_ID)
}

/// Epoch guards in `code`, counting both encodings by resolving each to the
/// epoch's address (the same census `x64/tests.rs::layout_epoch_guards` runs).
fn layout_epoch_guards(code: &[u8]) -> usize {
    let (epoch, _) = cratonvm_types::layout_replace_epoch_guard();
    let epoch = epoch as usize;
    let base = code.as_ptr() as usize;
    let mut n = 0usize;
    for i in 0..code.len() {
        // CMP DWORD [rip+disp32], imm32 — 81 3D <disp32> <imm32>.
        if i + 10 <= code.len() && code[i] == 0x81 && code[i + 1] == 0x3D {
            let d = i32::from_le_bytes([code[i + 2], code[i + 3], code[i + 4], code[i + 5]]);
            if base
                .wrapping_add(i)
                .wrapping_add(10)
                .wrapping_add(d as isize as usize)
                == epoch
            {
                n += 1;
            }
        }
        // MOV R11, imm64(epoch address) — the fallback form's first instruction.
        if i + 10 <= code.len() && code[i] == 0x49 && code[i + 1] == 0xBB {
            let mut imm = [0u8; 8];
            imm.copy_from_slice(&code[i + 2..i + 10]);
            if u64::from_le_bytes(imm) as usize == epoch {
                n += 1;
            }
        }
    }
    n
}

fn guard_switched_off() -> bool {
    std::env::var("CRATONVM_JIT_SP_FIELD_LAYOUT_GUARD").is_ok_and(|v| v == "0")
}

fn compile_site(
    name: &str,
    descriptor: &str,
    code: &[u8],
    nargs: usize,
    ret: u8,
) -> CompiledMethod {
    let entry =
        try_resolve_string_intrinsic("java/lang/String", name, descriptor, Some(string_layout()))
            .expect("String intrinsic must register with a layout")
            .0;
    compile(
        code,
        code.len(),
        nargs,
        nargs,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            nargs,
            JitDirectCall {
                entry,
                needs_context: false,
                num_params: nargs - 1, // receiver excluded
                return_type: ret,
                guard_class_id: 0,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers(),
        HashSet::new(),
        HashMap::new(),
        Some(string_layout()),
    )
    .expect("String intrinsic wrapper must compile")
}

/// STRING_ACCESS: `int f(String s) { return s.length(); }`.
#[test]
fn string_length_intrinsic_guards_the_layout_epoch() {
    if guard_switched_off() {
        return;
    }
    // aload_0, invokevirtual #1, ireturn
    let code = [0x2a, 0xb6, 0x00, 0x01, 0xac, 0, 0];
    let m = compile_site("length", "()I", &code, 1, b'I');
    // Resolved against the LIVE buffer: the RIP form is position-dependent.
    assert_eq!(layout_epoch_guards(m.code_bytes()), 1);
}

/// STRING_SEARCH: `boolean f(String s, Object o) { return s.equals(o); }`.
#[test]
fn string_equals_intrinsic_guards_the_layout_epoch() {
    if guard_switched_off() {
        return;
    }
    // aload_0, aload_1, invokevirtual #1, ireturn
    let code = [0x2a, 0x2b, 0xb6, 0x00, 0x01, 0xac, 0, 0];
    let m = compile_site("equals", "(Ljava/lang/Object;)Z", &code, 2, b'Z');
    assert_eq!(layout_epoch_guards(m.code_bytes()), 1);
}
