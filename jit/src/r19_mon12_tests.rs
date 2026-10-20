// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 19 proposal wave 12, lane mon12 (MON11-1 of
//! `docs/internal/jit-proposals/jit-r19-mon11-proposals-RETIRED-20261009.md`): does the optimizing
//! tier coarsen javac's EXACT bytecode for `R19Mon10Rows.methodLoop`
//! (`for (..) { synchronized (LOCK) { methodCount++; } }` with its two
//! catch-any rows), or is the 12.5 ns row a tier the coarsening never sees?

use crate::r19_syncdoor3_tests::{method, row};
use crate::{CachedBytecodeMethod, CompiledMethod};

/// javac 25: `static void methodLoop(int n) { for (int i = 0; i < n; i++) {
/// synchronized (LOCK) { methodCount++; } } }` (cp 7 `LOCK`, cp 20
/// `methodCount`). Header at bci 2; rows `[13, 23) -> 26`, `[26, 29) -> 26`.
const METHOD_LOOP: [u8; 38] = [
    0x03, 0x3c, // 0: iconst_0; istore_1
    0x1b, 0x1a, 0xa2, 0x00, 0x21, // 2: iload_1; iload_0; if_icmpge 37
    0xb2, 0x00, 0x07, 0x59, 0x4d, 0xc2, // 7: getstatic LOCK; dup; astore_2; monitorenter
    0xb2, 0x00, 0x14, 0x04, 0x60, 0xb3, 0x00, 0x14, // 13: methodCount++
    0x2c, 0xc3, 0xa7, 0x00, 0x08, // 21: aload_2; monitorexit; goto 31
    0x4e, 0x2c, 0xc3, 0x2d, 0xbf, // 26: astore_3; aload_2; monitorexit; aload_3; athrow
    0x84, 0x01, 0x01, 0xa7, 0xff, 0xe0, // 31: iinc 1, 1; goto 2
    0xb1, // 37: return
];

const HOLDER: u32 = 9;

/// The statics cell address the fake resolver hands out (never dereferenced:
/// the body is never run; a constant, so the test adds no `static`).
const FAKE_STATICS_CELL: i64 = 0x7000_0000;

extern "C" fn static_base(_ctx: i64, class_id: i64, _field_index: i64) -> i64 {
    if class_id == i64::from(HOLDER) {
        FAKE_STATICS_CELL
    } else {
        0
    }
}

fn method_loop() -> CachedBytecodeMethod {
    let mut m = method(
        "methodLoop",
        &METHOD_LOOP,
        vec![row(13, 23, 26, 0), row(26, 29, 26, 0)],
        2,
        4,
    );
    m.is_synchronized = false;
    m.is_static = true;
    m.method_descriptor = std::sync::Arc::from("(I)V");
    m
}

pub(crate) fn compile_method_loop(
    optimize: bool,
    edits: &[(&str, Option<&str>)],
) -> Option<CompiledMethod> {
    let cached = method_loop();
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a
    // `usize`, so all-zero is a valid value; never run.
    let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.monitor_enter = 0x5000_0000;
    h.monitor_exit = 0x5000_0040;
    h.monitor_block_offset_in_thread = 0x28;
    h.class_id_offset_in_obj = 8;
    h.getstatic = 0x5000_0080;
    h.putstatic_int = 0x5000_00C0;
    h.putstatic_object = 0x5000_0100;
    let statics = |cp: u16| -> Option<(u32, usize, u8, bool)> {
        match cp {
            7 => Some((HOLDER, 0, b'L', false)),
            20 => Some((HOLDER, 1, b'I', false)),
            _ => None,
        }
    };
    let finals = |cp: u16| cp == 7;
    let direct = crate::DirectHelperTable {
        monitor_enter: 0x5000_0000,
        monitor_exit: 0x5000_0040,
        static_base_resolver: static_base as *const () as usize,
        static_base_resolver_ctx: 0x1000,
        ..crate::DirectHelperTable::EMPTY
    };
    let verdicts = crate::JitVerdictRegistry::default();
    let retries = crate::jfr_compile_decision::DeferredNewRetries::default();
    let mut req = crate::compile_request::CompileRequest::new(
        &cached,
        &h,
        crate::compile_request::CompileRealm {
            verdicts: &verdicts,
            deferred_new_retries: &retries,
        },
    );
    req.cp_static_field_resolver = Some(&statics);
    req.cp_static_field_final_resolver = Some(&finals);
    req.direct_helpers = &direct;
    req.optimize = optimize;
    cratonvm_types::flags::with_thread_overrides(edits, || crate::try_compile_request(&req))
}

/// `LOCK CMPXCHG [R11 + MARK_WORD_OFFSET], ECX`: one inline thin-lock site.
pub(crate) fn inline_cas_count(cm: &CompiledMethod) -> usize {
    // Cast: pinned < 128 by the emitter's own const assertion.
    let mark = cratonvm_types::MARK_WORD_OFFSET as u8;
    let pat = [0xF0, 0x41, 0x0F, 0xB1, 0x4B, mark];
    cm.code_bytes()
        .windows(pat.len())
        .filter(|w| *w == pat)
        .count()
}

/// The finding MON11-1 rests on: the optimizing tier takes javac's own
/// `methodLoop` (table and all), but its block coarsening refused the body's
/// direct `putstatic` ("a body node could call, ..."), so the default body
/// keeps one inline thin-lock pair per iteration. Under
/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_STATIC_STORES=1` the loop is coarsened:
/// the fast pair and the identity exits add inline sites.
#[test]
fn javacs_method_loop_is_coarsened_only_with_static_stores_admitted() {
    const ON: &[(&str, Option<&str>)] =
        &[("CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_STATIC_STORES", Some("1"))];
    let sp = compile_method_loop(false, &[]).expect("single-pass compiles methodLoop");
    // Default ON since the wave 12 integration: the old body is `=0`.
    let default = compile_method_loop(
        true,
        &[("CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_STATIC_STORES", Some("0"))],
    );
    let on = compile_method_loop(true, ON);
    eprintln!(
        "[mon12] single-pass sites={}; optimizing default: ir={:?} sites={:?}; static stores: ir={:?} sites={:?}",
        inline_cas_count(&sp),
        default.as_ref().map(|c| c.used_ir_backend),
        default.as_ref().map(inline_cas_count),
        on.as_ref().map(|c| c.used_ir_backend),
        on.as_ref().map(inline_cas_count),
    );
    let (Some(default), Some(on)) = (default, on) else {
        panic!("the optimizing tier refused javac's methodLoop");
    };
    assert!(default.used_ir_backend && on.used_ir_backend);
    assert_eq!(
        inline_cas_count(&default),
        inline_cas_count(&sp),
        "default: the optimizing body has one inline site per bytecode monitor op"
    );
    assert!(
        inline_cas_count(&on) > inline_cas_count(&default),
        "the switch lets the block coarsening take javac's own bytecode"
    );
}
