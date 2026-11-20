// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT tier2 wave 4, page agent `bar21` (the JIT r21 "barriers" lane page): pins of the
//! GC barrier contracts of compiled reference stores that the wave-4 review of the
//! emitters relied on. Run them with `cargo test -p cratonvm-jit bar21`.
//!
//! * The G1 `aastore` screen against a POISONED `JIT_G1_BARRIER` table (EXECUTED). The
//!   collector poisons the table when a second G1 heap shares the process or when the
//!   owner is dropped (`gc::gen_heap::poison_jit_g1_barrier_words`: length 0, region mask
//!   all ones, base 0). Code compiled before the poisoning keeps reading the table at run
//!   time, so every DISTINCT holder / value pair must read as "another region" (the lean
//!   `jit_g1_post_write_barrier` call, which resolves its collector from `vm_ptr`) and
//!   only a null value or the very same object may skip the barrier.
//! * No compiled intrinsic stores a reference into an array: `Arrays.fill`, `copyOf`,
//!   `copyOfRange`, `sort` and `setAll` of reference arrays run the JDK body, whose element
//!   stores are `aastore`s with their own barriers.
//! * Source-order pins of the barrier emitters: the SATB gate before the old-element load
//!   and the store, the post barrier after the store, the ZGC read-barrier veto and the gates
//!   before the raw word copy of the inline reference `System.arraycopy`.

#[cfg(target_arch = "x86_64")]
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

#[cfg(all(windows, target_arch = "x86_64"))]
const ARG0: u8 = 1; // RCX
#[cfg(all(windows, target_arch = "x86_64"))]
const ARG1: u8 = 2; // RDX
#[cfg(all(not(windows), target_arch = "x86_64"))]
const ARG0: u8 = 7; // RDI
#[cfg(all(not(windows), target_arch = "x86_64"))]
const ARG1: u8 = 6; // RSI

/// `MOV dst, src` (64-bit).
#[cfg(target_arch = "x86_64")]
fn mov(dst: u8, src: u8) -> [u8; 3] {
    [
        0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8),
        0x89,
        0xC0 | ((src & 7) << 3) | (dst & 7),
    ]
}

/// The poisoned table is never live, so no NEW compile emits the inline G1 `aastore`,
/// and the already-compiled screen sends every distinct pair to the lean barrier.
///
/// The fixture only compares addresses: the screen never dereferences the array or the
/// value, it reads the gate byte and the two table words. The addresses are far above any
/// plausible region count (`>> 20` is over 100 million), so the optional young-holder
/// filter (`JIT_G1_YOUNG`, process-wide, published only by a live G1 collector) cannot
/// turn a cross-region answer into a skip even if another test in the process published it.
#[cfg(target_arch = "x86_64")]
#[test]
fn bar21_a_poisoned_g1_table_sends_every_distinct_pair_to_the_lean_barrier() {
    static TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    static GATE: AtomicU8 = AtomicU8::new(0);
    // `poison_jit_g1_barrier_words`: [1] = 0, [2] = usize::MAX, [0] = 0, [3] = 0, [4] = 0.
    TABLE[1].store(0, Ordering::Release);
    TABLE[2].store(usize::MAX, Ordering::Release);
    TABLE[0].store(0, Ordering::Release);
    TABLE[3].store(0, Ordering::Release);
    TABLE[4].store(0, Ordering::Release);
    GATE.store(0, Ordering::Release);
    let plan = crate::x64::G1AastorePlan {
        pre: &GATE as *const AtomicU8 as usize, // Cast: the gate's address
        table: TABLE.as_ptr() as usize,         // Cast: the table's address
        post: 0x1234_5678,                      // never called: the screen only branches
    };
    assert!(
        !crate::x64::g1_barrier_table_live(plan.table),
        "a poisoned table is not live"
    );
    assert_eq!(
        crate::x64::g1_aastore_plan_from(plan.pre, false, false, plan.table, plan.post),
        None,
        "no new compile emits the inline G1 aastore against a poisoned table"
    );

    // The bytes a compile made BEFORE the poisoning emitted: they read the table at run time.
    let (bytes, decline, cross) = crate::ir_lower::g1_aastore_screen_bytes(plan);
    assert_eq!(decline.len(), 1, "one decline edge: the armed gate");
    assert_eq!(cross.len(), 1, "one cross-region edge");
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&mov(0, ARG0)); // RAX = array
    body.extend_from_slice(&mov(2, ARG1)); // RDX = value
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET (plain: no barrier)
    let cross_at = body.len();
    body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
    let decline_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -1 ; RET
    let mut patch = |field: usize, to: usize| {
        let operand = at + field;
        let rel = i32::try_from(to).expect("small") - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    };
    for d in decline {
        patch(d, decline_at);
    }
    for c in cross {
        patch(c, cross_at);
    }
    let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: a complete `extern "C" fn(u64, u64) -> i64` that reads only GATE and TABLE
    // (statics) and writes RAX, RCX, RDX, R10, R11 (caller-saved on both ABIs).
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> i64>(buf.as_ptr())
    };
    let ask = |a: usize, v: usize| f(a as u64, v as u64); // Cast: test addresses

    let a: usize = 0x0000_7F12_3456_0000;
    let b: usize = 0x0000_7F65_4321_0000;
    let cases: [(usize, usize, i64, &str); 5] = [
        (a, 0, 0, "a null value records no edge"),
        (a, a, 0, "the same object needs no edge"),
        (a, a + 0x40, 1, "even a neighbour is 'another region' once poisoned"),
        (a, b, 1, "two distant objects"),
        (b, a, 1, "the other way"),
    ];
    for (array, value, want, why) in cases {
        assert_eq!(ask(array, value), want, "gate clear: {why}");
    }
    GATE.store(1, Ordering::Release);
    for (array, value, _, why) in cases {
        assert_eq!(ask(array, value), -1, "gate armed: jit_aastore logs the old element ({why})");
    }
    GATE.store(0, Ordering::Release);
}

/// A reference-array store is an `aastore` of the JDK body, not an intrinsic: nothing in the
/// resolver may bind a reference-array fill, copy, sort or setAll to inline bytes that would
/// need their own barrier. The `([II)V` control proves the harness reaches the Arrays family.
#[test]
fn bar21_no_compiled_intrinsic_stores_a_reference_into_an_array() {
    assert!(
        crate::try_resolve_intrinsic("java/util/Arrays", "fill", "([II)V").is_some(),
        "control: the primitive fill is an intrinsic"
    );
    for (name, desc) in [
        ("fill", "([Ljava/lang/Object;Ljava/lang/Object;)V"),
        ("fill", "([Ljava/lang/Object;IILjava/lang/Object;)V"),
        ("copyOf", "([Ljava/lang/Object;I)[Ljava/lang/Object;"),
        ("copyOfRange", "([Ljava/lang/Object;II)[Ljava/lang/Object;"),
        ("sort", "([Ljava/lang/Object;)V"),
        ("setAll", "([Ljava/lang/Object;Ljava/util/function/IntFunction;)V"),
    ] {
        assert!(
            crate::try_resolve_intrinsic("java/util/Arrays", name, desc).is_none(),
            "Arrays.{name}{desc} must not be an intrinsic (its element stores need barriers)"
        );
    }
}

fn index_of(src: &str, needle: &str) -> usize {
    src.find(needle).unwrap_or_else(|| panic!("missing: {needle}"))
}

/// The function body that starts at `head` and ends at the first four-space closing brace.
fn fn_body<'s>(src: &'s str, head: &str) -> &'s str {
    let body = src
        .split(head)
        .nth(1)
        .unwrap_or_else(|| panic!("missing: {head}"));
    &body[..body.find("\n    }\n").expect("the end of the function")]
}

/// Single-pass `aastore` (0x53): the SATB gate, then the old-element load and the pre-barrier
/// call, then the store, then the post barrier. Every reference element store of this tier is one
/// of exactly three `emit_ref_astore_regs()` sites (the ZGC colored arm, the ZGC fused-gate arm
/// and the main sequence); a new one has to be reviewed for its barrier and bump the count here.
#[test]
fn bar21_the_single_pass_aastore_orders_gate_load_call_store_post() {
    let src = include_str!("x64/op_array.rs").replace("\r\n", "\n");
    assert_eq!(
        src.matches("self.emit_ref_astore_regs();").count(),
        3,
        "a new reference element store in the single-pass aastore needs a barrier review"
    );
    let gate = index_of(&src, "self.emit_sp_aastore_satb_gate_skip(pre)");
    let old = index_of(&src, "self.emit_ref_aload_regs(); // RAX = OLD ref value");
    let pre_call = index_of(&src, "self.emit_call_absolute(self.helpers.satb_pre_write_barrier);");
    let store_note = index_of(
        &src,
        "// Inline store: MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX",
    );
    let store = store_note
        + index_of(&src[store_note..], "self.emit_ref_astore_regs();");
    let post_note = index_of(&src, "// Post-store publication.");
    let g1_post = index_of(
        &src,
        "self.emit_g1_post_write_barrier_regs(RAX, RDX, RCX, array_slot, val_slot);",
    );
    assert!(gate < old, "the SATB gate comes before the old-element load");
    assert!(old < pre_call, "the old element is loaded before the pre-barrier call");
    assert!(pre_call < store_note && store_note < store, "the pre-barrier precedes the store");
    assert!(store < post_note && post_note < g1_post, "the G1 post barrier follows the store");
    for (at, _) in src.match_indices("self.emit_call_absolute(self.helpers.write_barrier);") {
        assert!(at > store, "a write_barrier call before the element store (at {at})");
    }
}

/// The optimizing tier's G1 and generational `aastore` stores, and the inline reference
/// `System.arraycopy` prefix: the gate comes first, the barrier-free word stores second.
#[test]
fn bar21_the_ir_aastore_and_ref_arraycopy_gate_before_they_store() {
    let src = concat!(include_str!("ir_lower/mod.rs"), "\n", include_str!("ir_lower/virtual_object_info.rs"), "\n", include_str!("ir_lower/mir_plan_and_init.rs"), "\n", include_str!("ir_lower/vector_preloop.rs"), "\n", include_str!("ir_lower/value_phi_copies.rs"), "\n", include_str!("ir_lower/field_proof_entry_is_rechecked_osr.rs"), "\n", include_str!("ir_lower/int_expr_bounds.rs"), "\n", include_str!("ir_lower/character_to_chars_prefix.rs"), "\n", include_str!("ir_lower/getfield_legacy_read.rs"), "\n", include_str!("ir_lower/post_call_exit_site_for.rs"), "\n", include_str!("ir_lower/entry_fold_node.rs"), "\n", include_str!("ir_lower/call_loaded_ic_entry_releasing.rs"), "\n", include_str!("ir_lower/fallthrough_poll_spills.rs"), "\n", include_str!("ir_lower/ir_ic_cascade_admits_kind.rs"), "\n", include_str!("ir_lower/implicit_kind_matches_catch.rs"), "\n", include_str!("ir_lower/scoped_raw_tail.rs"), "\n", include_str!("ir_lower/frame_needs.rs"), "\n", include_str!("ir_lower/back_edge_dead_ref_homes.rs"), "\n", include_str!("ir_lower/block_lazy_clear_survivors.rs"), "\n", include_str!("ir_lower/promoted_pending_mark.rs"), "\n", include_str!("ir_lower/site_value_live_at.rs"), "\n", include_str!("ir_lower/plan_register_residency_with.rs"), "\n", include_str!("ir_lower/inner_attempt.rs"), "\n", include_str!("ir_lower/g1_aastore_screen.rs"), "\n", include_str!("ir_lower/scoped_session_confined_rows.rs")).replace("\r\n", "\n");

    // G1: screen (SATB gate, null, same region), two stores, the edge filter, the lean call.
    let g1 = fn_body(&src, "fn emit_g1_ir_aastore(");
    assert_eq!(
        g1.matches("self.emit_gpr_array_elem_store(MemKind::Long, ArrayStoreSource::Reg(RDX));").count(),
        2,
        "the plain store and the cross-region store"
    );
    let screen = index_of(g1, "g1_aastore_screen_bytes(plan)");
    let first_store = index_of(g1, "self.emit_gpr_array_elem_store(MemKind::Long, ArrayStoreSource::Reg(RDX));");
    let second_store = first_store
        + 1
        + index_of(&g1[first_store + 1..], "self.emit_gpr_array_elem_store(MemKind::Long, ArrayStoreSource::Reg(RDX));");
    let edge = index_of(g1, "g1_edge_filter_hit_bytes");
    let call = index_of(g1, "self.emit_mov_reg_imm64(RAX, plan.post as u64);");
    assert!(screen < first_store, "the screen precedes every store");
    assert!(second_store < edge && edge < call, "the barrier follows the cross-region store");

    // Generational / ZGC gated: the SATB gate before the one inline store.
    let gated = fn_body(&src, "fn emit_gated_ir_aastore(");
    let pre_gate = index_of(gated, "to_helper.push(self.emit_ir_satb_pre_gate(pre, gate_once));");
    let store_marker = index_of(gated, "// The barrier-free store.");
    let store = index_of(gated, "self.emit_gpr_array_elem_store(MemKind::Long, ArrayStoreSource::Reg(RDX));");
    assert!(pre_gate < store_marker && store_marker < store);

    // Inline reference arraycopy: the armed-ZGC veto and the collector gates precede the word copy.
    let copy = fn_body(&src, "fn try_emit_ir_ref_arraycopy_prefix(");
    let veto = index_of(copy, "crate::x64::zgc_read_barrier_blocks_inline_fields()");
    let satb = index_of(copy, "decline.push(self.emit_ir_satb_pre_gate(pre, false));");
    let words = index_of(copy, "self.emit_ir_ref_copy_words_unrolled(");
    assert!(veto < satb && satb < words, "the veto and the SATB gate precede the raw copy");
}
