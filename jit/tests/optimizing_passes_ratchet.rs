// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Ratchet: the single-pass tier's optimizing passes must still ENGAGE
//! (Finding #68).
//!
//! Contract: `optimizing-passes-still-exist-in-both-tiers-20260912.md`.
//!
//! # What this used to assert, and why that was not a ratchet
//!
//! Until 2026-09-17 this file's only substantive test read `jit/src/x64/` with
//! `std::fs::read_dir` and asserted that seven FILENAMES were present:
//! `bce.rs`, `escape_analysis.rs`, `inlining.rs`, `licm.rs`, `licm_int.rs`,
//! `loop_unroll_admission.rs`, `null_check_elim.rs`.
//!
//! A filename is not a pass. That assertion passes over a `bce.rs` whose entry
//! point returns early on its first line, over a `licm.rs` no caller reaches,
//! and over a module deleted and replaced by an empty file of the same name.
//! It fails only for a *rename* — the one edit guaranteed to be deliberate,
//! and the one the compiler catches anyway. It was, precisely, a test of the
//! directory listing.
//!
//! # What it asserts now
//!
//! The backend has a switch that turns the speculative passes off:
//! `BackendRequest::baseline_mode` (and `CRATONVM_JIT_BASELINE_FAST`). That
//! switch is the census instrument this file was missing. For a shape the
//! passes have something to do with, the two modes must produce DIFFERENT
//! code — which is what "the pass still engages" means operationally, and
//! which no amount of a pass being present-but-inert satisfies.
//!
//! The negative fixture is what makes the positive one mean something. A
//! method the passes can do nothing with — `return a;` — must compile
//! IDENTICALLY in both modes. Without it, a change that made `baseline_mode`
//! alter the prologue unconditionally would keep the positive assertion green
//! while saying nothing at all about the passes.
//!
//! # What this file still does not assert
//!
//! *Which* pass engaged. The single-pass passes publish no per-pass census
//! counters (`grep AtomicU64 jit/src/x64/bce.rs` is empty; the same for
//! `licm.rs` and `escape_analysis.rs`), so "some optimizing work happened" is
//! the strongest statement available from outside the crate. Adding
//! `(engaged, declined)` pairs to those three modules — the shape
//! `null_check_elim::receiver_null_check_counts` already has, and for exactly
//! the reason its doc gives — would let this file name the pass instead of the
//! aggregate. Recorded in `NOTES-runtime.md`.
//!
//! Also: this file compares LENGTHS, not bytes. Two artifacts are separately
//! mapped, and `emit_call_absolute` picks `rel32` or `MOV imm64` by distance,
//! so the emitted bytes legitimately depend on where each buffer landed. The
//! fabricated helper addresses below are far from anything this process maps,
//! which pins the `imm64` form for both and makes the length a function of the
//! bytecode alone.

use std::collections::{HashMap, HashSet};

use cratonvm_jit::x64::{compile_with_request, BackendRequest};
use cratonvm_jit_api::JitRuntimeHelpers;

/// A fabricated helper address, deliberately far from anything this process
/// maps. See the module header: it pins the `MOV RAX, imm64` call form, which
/// is what makes a length comparison between two separately mapped artifacts
/// meaningful. Nothing compiled here is ever executed.
const FAKE_HELPER: usize = 0x0000_4242_0000_0000;

fn fake_helpers() -> JitRuntimeHelpers {
    let h = FAKE_HELPER;
    JitRuntimeHelpers {
        newarray: h,
        new_object: h,
        anewarray_object: h,
        baload: h,
        bastore: h,
        iaload: h,
        iastore: h,
        aaload: h,
        aastore: h,
        multianewarray_2d: h,
        arraylength: h,
        getfield: h,
        putfield_int: h,
        putfield_long: h,
        putfield_float: h,
        putfield_double: h,
        putfield_object: h,
        getstatic: h,
        putstatic_int: h,
        putstatic_long: h,
        putstatic_float: h,
        putstatic_double: h,
        putstatic_object: h,
        checkcast: h,
        instanceof_check: h,
        throw_aioobe: h,
        throw_arithmetic: h,
        invoke_dispatch: h,
        invoke_virtual_mic: h,
        write_barrier: h,
        satb_pre_write_barrier: h,
        uncommon_trap: h,
        throw_exception: h,
        jit_npe_with_action: h,
        dispatch_threw: h,
        set_throw_bci: h,
        service_callee_deopt: h,
        ldc_string: h,
        ..Default::default()
    }
}

/// Compile `code` through the single-pass backend with the speculative passes
/// on or off, and report the emitted code length.
///
/// `None` means the backend refused, which for these shapes is itself a
/// failure: a refusal in one mode and not the other would make the comparison
/// vacuous, and a refusal in both would make every length equal.
fn emitted_len(code: &[u8], num_params: usize, max_locals: usize, baseline: bool) -> Option<usize> {
    let backend = BackendRequest {
        baseline_mode: baseline,
        ..BackendRequest::default()
    };
    let helpers = fake_helpers();
    compile_with_request(
        backend,
        Vec::new(), // compact_field_info
        code,
        code.len(),
        num_params,
        max_locals,
        false,      // needs_heap
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // anewarray_info
        Vec::new(), // invoke_info
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &helpers,
        HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .map(|m| m.code_len())
}

/// `static int sum(int[] a, int n) { int s = 0; for (int i = 0; i < n; i++) s += a[i]; return s; }`
///
/// The canonical shape the speculative passes exist for: a counted loop whose
/// array index is the induction variable, a loop-invariant array reference,
/// and a null check on that reference that dominates the loop.
fn counted_array_sum() -> Vec<u8> {
    vec![
        0x03, // 0:  iconst_0
        0x3d, // 1:  istore_2      (s = 0)
        0x03, // 2:  iconst_0
        0x3e, // 3:  istore_3      (i = 0)
        0x1d, // 4:  iload_3
        0x1b, // 5:  iload_1
        0xa2, 0x00, 0x0f, // 6:  if_icmpge +15 -> 21
        0x1c, // 9:  iload_2
        0x2a, // 10: aload_0
        0x1d, // 11: iload_3
        0x2e, // 12: iaload
        0x60, // 13: iadd
        0x3d, // 14: istore_2
        0x84, 0x03, 0x01, // 15: iinc 3, 1
        0xa7, 0xff, 0xf2, // 18: goto -14 -> 4
        0x1c, // 21: iload_2
        0xac, // 22: ireturn
    ]
}

/// `static int identity(int a) { return a; }` — the negative fixture. No loop
/// to hoist out of, no array access to bounds-check, no allocation to
/// scalar-replace, no call to inline.
fn identity() -> Vec<u8> {
    vec![0x1a, 0xac]
}

/// The positive half: with something to optimise, the two modes must diverge.
#[test]
fn the_speculative_passes_change_the_emitted_code_for_a_shape_they_can_act_on() {
    let code = counted_array_sum();
    let optimizing = emitted_len(&code, 2, 4, false)
        .expect("the counted array sum must compile with the speculative passes ON");
    let baseline = emitted_len(&code, 2, 4, true)
        .expect("the counted array sum must compile with the speculative passes OFF");
    assert_ne!(
        optimizing, baseline,
        "`BackendRequest::baseline_mode` made no difference to a counted loop over an \
         int[] — the shape bounds-check elimination, loop-invariant hoisting and \
         receiver null-check elimination all exist for. Every optimizing pass in \
         jit/src/x64/ is therefore either unreached from this path or inert. The \
         predecessor of this test asserted that seven FILENAMES were present in \
         jit/src/x64/, which seven empty files satisfy; this is the assertion that \
         they do not. (optimizing={optimizing} bytes, baseline={baseline} bytes)",
    );
}

/// The negative half, and the reason the positive half means anything: with
/// nothing to optimise, the two modes must agree byte-count for byte-count.
///
/// If this fails, `baseline_mode` has grown an effect that is not about the
/// speculative passes — a different prologue, a different frame layout, a
/// different safepoint policy — and the test above is then measuring that
/// instead, whether or not a single pass still engages.
#[test]
fn baseline_mode_changes_nothing_for_a_shape_with_nothing_to_optimise() {
    let code = identity();
    let optimizing =
        emitted_len(&code, 1, 1, false).expect("`return a;` must compile in optimizing mode");
    let baseline =
        emitted_len(&code, 1, 1, true).expect("`return a;` must compile in baseline mode");
    assert_eq!(
        optimizing, baseline,
        "`baseline_mode` changed the code emitted for `return a;`, which has nothing for \
         any speculative pass to do. Whatever that difference is, it is not a pass \
         engaging — and it leaves the positive test above unable to tell the two apart. \
         (optimizing={optimizing} bytes, baseline={baseline} bytes)",
    );
}

/// Both modes must actually produce a body for every shape measured above. A
/// refusal in both modes is the silent case: it would make every length equal
/// and the negative test green for the wrong reason.
#[test]
fn both_modes_compile_every_shape_this_file_measures() {
    for (name, code, params, locals) in [
        ("countedArraySum", counted_array_sum(), 2usize, 4usize),
        ("identity", identity(), 1, 1),
    ] {
        for baseline in [false, true] {
            let len = emitted_len(&code, params, locals, baseline).unwrap_or_else(|| {
                panic!("{name}: the backend refused to compile with baseline_mode={baseline}")
            });
            assert!(
                len > 0,
                "{name}: an empty body with baseline_mode={baseline}"
            );
        }
    }
}

#[test]
fn backend_request_default_mode_is_compatible_not_baseline() {
    let req = BackendRequest::default();
    assert!(
        !req.baseline_mode,
        "BackendRequest::default() must remain compatible (not baseline) by default"
    );
}

#[test]
fn backend_request_baseline_mode_field_is_configurable() {
    let req = BackendRequest {
        baseline_mode: true,
        ..BackendRequest::default()
    };
    assert!(req.baseline_mode);
}
