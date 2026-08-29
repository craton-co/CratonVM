// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A differ for the x86-64 backend: compile a fixed corpus of methods and
//! fingerprint everything each compile publishes.
//!
//! # What this is for
//!
//! Behavioural tests answer "does the compiled code still compute the right
//! value". They do not answer "did this change alter what the backend emits",
//! and for a backend that is the question that matters: a refactor is supposed
//! to change nothing, and an optimisation is supposed to change something
//! specific. This corpus makes both answerable.
//!
//! ```text
//! cargo test -p cratonvm-jit --test x64_artifact_corpus -- --nocapture dump_corpus \
//!   | grep ^CORPUS > /tmp/before.txt
//! # ... make the change ...
//! cargo test -p cratonvm-jit --test x64_artifact_corpus -- --nocapture dump_corpus \
//!   | grep ^CORPUS > /tmp/after.txt
//! diff -u /tmp/before.txt /tmp/after.txt
//! ```
//!
//! Each line is `name`, `OK`/`REFUSED`, code length, a hash of the emitted
//! bytes, and a hash of the metadata published beside them. A pure refactor
//! must produce an empty diff. An optimisation should produce a diff whose
//! *shape* you can predict before running it.
//!
//! No expected values are checked in. Golden hashes would have to be updated by
//! every legitimate codegen change, and a golden file that is routinely
//! regenerated stops being evidence. The artifact is the comparison, not the
//! constant.
//!
//! # Why the hashes are stable
//!
//! Every address the backend can bake into emitted code is a **constant chosen
//! here**, not a real function pointer: the helper table is fabricated by
//! `fake_helpers`, and nothing in the corpus is ever executed. Without that,
//! the emitted bytes would embed the test binary's own layout and differ
//! between runs under ASLR, which would make a hash comparison meaningless
//! rather than merely noisy.
//!
//! # What the fingerprint covers, and what it cannot
//!
//! `metadata_hash` covers the OSR entry table and its assignment vectors and
//! dead-local masks, the frame layout and the slot offsets other components
//! address, the oop maps, and the deopt points. Mutation-tested on 2026-08-03:
//! deleting the OSR high-half nulling moves 36 cases, flattening the dead-local
//! mask refinement moves 13, and an off-by-one in `osr_num_reg_locals` moves
//! 151.
//!
//! One path it cannot reach on this platform: **XMM local homes are
//! `#[cfg(windows)]`-only** (`x64.rs`, the SysV post-call XMM spill/reload gap),
//! so on a Linux build `xmm_assignments` is all-`None` and the XMM half of the
//! OSR high-half nulling and of the dead-local mask is statically inert. A
//! mutation there is invisible here for that reason and not because the corpus
//! is thin — the same mutation on a Windows build is expected to show up.
//!
//! # Adding cases
//!
//! Add shapes, not opcodes. A case earns its place by reaching a decision the
//! corpus does not already reach — a different peephole, a different admission
//! outcome, a different metadata shape.
//! `corpus_actually_exercises_the_published_metadata` is the floor: it fails if
//! the corpus stops reaching OSR entries, oop maps, register-homed locals or a
//! category-2 local at a published entry, which is what keeps the metadata
//! column from quietly becoming a constant.

#![allow(clippy::too_many_arguments)]

use cratonvm_jit::x64::compile;
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Deterministic fake helper table
// ---------------------------------------------------------------------------

/// Canonical-looking user-space addresses, one per helper slot, assigned in a
/// fixed order. Nothing is ever called, only baked.
fn fake_helpers(inline_tlab: bool) -> JitRuntimeHelpers {
    let mut n = 0usize;
    let mut a = || {
        n += 1;
        0x0000_7F10_0000_0000usize + n * 0x40
    };
    JitRuntimeHelpers {
        safepoint_flag_addr: a(),
        safepoint_slow_path: a(),
        jit_card_table_addr: a(),
        jit_card_old_base: 0x0000_7F20_0000_0000,
        jit_card_old_end: 0x0000_7F20_1000_0000,
        newarray: a(),
        new_object: a(),
        anewarray_object: a(),
        baload: a(),
        bastore: a(),
        iaload: a(),
        iastore: a(),
        aaload: a(),
        aastore: a(),
        multianewarray_2d: a(),
        arraylength: a(),
        getfield: a(),
        putfield_int: a(),
        putfield_long: a(),
        putfield_float: a(),
        putfield_double: a(),
        putfield_object: a(),
        getstatic: a(),
        putstatic_int: a(),
        putstatic_long: a(),
        putstatic_float: a(),
        putstatic_double: a(),
        putstatic_object: a(),
        checkcast: a(),
        instanceof_check: a(),
        throw_aioobe: a(),
        throw_arithmetic: a(),
        invoke_dispatch: a(),
        invoke_virtual_mic: a(),
        lambda_int_to_double: a(),
        write_barrier: a(),
        satb_pre_write_barrier: a(),
        uncommon_trap: a(),
        math_fma_double: a(),
        math_fma_float: a(),
        tlab_cursor_offset_in_thread: 0,
        tlab_end_offset_in_thread: 8,
        class_id_offset_in_obj: 0,
        get_current_thread: if inline_tlab { a() } else { 0 },
        tlab_post_init: if inline_tlab { a() } else { 0 },
        frame_record: 0,
        shadow_stack_offset_in_thread: 0,
        throw_exception: a(),
        jit_npe_with_action: a(),
        dispatch_threw: a(),
        jit_frem: a(),
        jit_drem: a(),
        self_call_stack_guard: 0,
        region_bounds_addr: 0,
        native_stack_floor_fn: 0,
        ldc_string: a(),
        set_throw_bci: a(),
        service_callee_deopt: a(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Case description
// ---------------------------------------------------------------------------

struct Case {
    name: String,
    code: Vec<u8>,
    num_params: usize,
    max_locals: usize,
    needs_heap: bool,
    inline_tlab: bool,
    multianewarray_info: Vec<(usize, i64)>,
    field_info: Vec<(usize, usize, u8)>,
    static_field_info: Vec<(usize, u32, usize, u8, bool)>,
    new_info: Vec<(usize, u32, usize, bool, bool)>,
    anewarray_info: Vec<(usize, u32)>,
    ldc_info: Vec<(usize, i64)>,
    ldc2w_info: Vec<(usize, i64)>,
    branch_hints: Vec<(usize, bool)>,
    loop_unroll_hints: Vec<(usize, usize)>,
}

impl Case {
    fn new(name: &str, params: usize, locals: usize, code: Vec<u8>) -> Case {
        Case {
            name: name.to_string(),
            code,
            num_params: params,
            max_locals: locals,
            needs_heap: false,
            inline_tlab: false,
            multianewarray_info: Vec::new(),
            field_info: Vec::new(),
            static_field_info: Vec::new(),
            new_info: Vec::new(),
            anewarray_info: Vec::new(),
            ldc_info: Vec::new(),
            ldc2w_info: Vec::new(),
            branch_hints: Vec::new(),
            loop_unroll_hints: Vec::new(),
        }
    }
    fn heap(mut self) -> Case {
        self.needs_heap = true;
        self
    }
    fn tlab(mut self) -> Case {
        self.inline_tlab = true;
        self
    }
    fn fields(mut self, v: Vec<(usize, usize, u8)>) -> Case {
        self.field_info = v;
        self
    }
    fn statics(mut self, v: Vec<(usize, u32, usize, u8, bool)>) -> Case {
        self.static_field_info = v;
        self
    }
    fn news(mut self, v: Vec<(usize, u32, usize, bool, bool)>) -> Case {
        self.new_info = v;
        self
    }
    fn anewarrays(mut self, v: Vec<(usize, u32)>) -> Case {
        self.anewarray_info = v;
        self
    }
    fn multis(mut self, v: Vec<(usize, i64)>) -> Case {
        self.multianewarray_info = v;
        self
    }
    fn ldcs(mut self, v: Vec<(usize, i64)>) -> Case {
        self.ldc_info = v;
        self
    }
    fn ldc2ws(mut self, v: Vec<(usize, i64)>) -> Case {
        self.ldc2w_info = v;
        self
    }
    fn hints(mut self, v: Vec<(usize, bool)>) -> Case {
        self.branch_hints = v;
        self
    }
    fn unroll(mut self, v: Vec<(usize, usize)>) -> Case {
        self.loop_unroll_hints = v;
        self
    }

    fn run(&self) -> (String, usize, u64, u64) {
        let helpers = fake_helpers(self.inline_tlab);
        let out = self.compile_with(&helpers);
        match out {
            None => ("REFUSED".to_string(), 0, 0, 0),
            Some(cm) => {
                let b = cm.code_bytes();
                ("OK".to_string(), b.len(), fnv1a(b), metadata_hash(&cm))
            }
        }
    }

    fn compile_with(&self, helpers: &JitRuntimeHelpers) -> Option<cratonvm_jit::CompiledMethod> {
        let code_len = self.code.len();
        compile(
            &self.code,
            code_len,
            self.num_params,
            self.max_locals,
            self.needs_heap,
            self.multianewarray_info.clone(),
            self.field_info.clone(),
            Vec::new(),
            self.static_field_info.clone(),
            self.new_info.clone(),
            self.anewarray_info.clone(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            self.ldc_info.clone(),
            self.ldc2w_info.clone(),
            self.branch_hints.iter().copied().collect::<HashMap<_, _>>(),
            self.loop_unroll_hints
                .iter()
                .copied()
                .collect::<HashMap<_, _>>(),
            helpers,
            HashSet::new(),
            HashMap::new(),
            None,
        )
    }
}

/// Hash everything a compile publishes *beside* the machine code.
///
/// `code_bytes` alone would not notice a damaged OSR entry table, a dropped
/// dead-local mask, a shifted frame-layout band, or a lost deopt point — none
/// of which live in the emitted instruction stream. Every field here is a
/// contract some other component reads: the OSR trampoline, the collector's
/// frame walker, or the deopt service.
fn metadata_hash(cm: &cratonvm_jit::CompiledMethod) -> u64 {
    let mut v: Vec<u8> = Vec::new();
    let mut i = |v: &mut Vec<u8>, x: i64| v.extend_from_slice(&x.to_le_bytes());

    // --- OSR entry metadata -------------------------------------------------
    match &cm.osr_pc_to_native {
        None => i(&mut v, -1),
        Some(t) => {
            i(&mut v, t.len() as i64);
            for e in t {
                i(&mut v, *e as i64);
            }
        }
    }
    i(&mut v, cm.osr_num_locals as i64);
    i(&mut v, cm.osr_num_reg_locals as i64);
    for opt in [&cm.osr_local_assignments, &cm.osr_xmm_assignments] {
        match opt {
            None => i(&mut v, -1),
            Some(t) => {
                i(&mut v, t.len() as i64);
                for e in t {
                    i(&mut v, e.map_or(-1, |r| r as i64));
                }
            }
        }
    }
    match &cm.osr_dead_mask {
        None => i(&mut v, -1),
        Some(t) => {
            i(&mut v, t.len() as i64);
            for e in t {
                i(&mut v, *e as i64);
            }
        }
    }
    for opt in [&cm.osr_callee_saved_regs, &cm.osr_callee_saved_xmms] {
        match opt {
            None => i(&mut v, -1),
            Some(t) => {
                i(&mut v, t.len() as i64);
                for e in t {
                    i(&mut v, *e as i64);
                }
            }
        }
    }
    for x in [
        cm.osr_frame_size,
        cm.osr_callee_saved_base,
        cm.osr_xmm_saved_base,
        cm.osr_heap_local_offset,
    ] {
        i(&mut v, x as i64);
    }
    i(&mut v, cm.osr_frame_record as i64);
    i(&mut v, cm.compiled_via_osr as i64);
    i(&mut v, cm.osr_exit_points.len() as i64);
    for p in &cm.osr_exit_points {
        i(&mut v, *p as i64);
    }
    i(&mut v, cm.can_osr_exit as i64);
    i(&mut v, cm.can_deopt_resume as i64);

    // --- frame layout and the slots other components address ----------------
    let f = &cm.frame_layout;
    for x in [
        f.java_locals_hi,
        f.ref_hoist_lo,
        f.ref_hoist_hi,
        f.arith_lo,
        f.arith_hi,
        f.scalar_lo,
        f.scalar_hi,
        f.locals_hi,
        f.spill_lo,
        f.spill_hi,
        f.callee_saved_lo,
        f.callee_saved_hi,
        f.xmm_saved_lo,
        f.xmm_saved_hi,
        f.reg_spill_lo,
        f.reg_spill_hi,
    ] {
        i(&mut v, x as i64);
    }
    for x in [
        cm.shadow_savebase_slot_off,
        cm.shadow_savetop_slot_off,
        cm.shadow_thread_slot_off,
        cm.shadow_off_in_thread,
        cm.jit_thread_slot_off,
        cm.stack_floor_slot_off,
    ] {
        i(&mut v, x as i64);
    }

    // --- GC and deopt metadata ---------------------------------------------
    i(&mut v, cm.oop_maps.len() as i64);
    for m in &cm.oop_maps {
        i(&mut v, m.native_pc_offset as i64);
        i(&mut v, m.bytecode_pc as i64);
        i(&mut v, m.live_frame_hi as i64);
        i(&mut v, m.moving_young_coverage_complete as i64);
        i(&mut v, m.frame_slot_offsets.len() as i64);
        for o in &m.frame_slot_offsets {
            i(&mut v, *o as i64);
        }
    }
    i(&mut v, cm.deopt_points.len() as i64);
    for d in &cm.deopt_points {
        i(&mut v, d.native_offset as i64);
        i(&mut v, d.bci as i64);
        i(&mut v, d.reason as i64);
        i(&mut v, d.action as i64);
        i(&mut v, d.speculation_id as i64);
    }
    i(&mut v, cm.has_dispatch as i64);
    i(&mut v, cm.used_ir_backend as i64);
    i(&mut v, cm.fully_oop_covered as i64);
    i(&mut v, cm.static_init_classes.len() as i64);
    i(&mut v, cm.inlined_methods.len() as i64);

    fnv1a(&v)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// Bytecode builders
// ---------------------------------------------------------------------------

/// `iload_0; iload_1; <op>; ireturn`
fn int_binop(op: u8) -> Vec<u8> {
    vec![0x1a, 0x1b, op, 0xac]
}
/// `lload_0; lload_2; <op>; lreturn`
fn long_binop(op: u8) -> Vec<u8> {
    vec![0x1e, 0x20, op, 0xad]
}
/// `lload_0; iload_2; <op>; lreturn` (long shifts take an int shift count)
fn long_shift(op: u8) -> Vec<u8> {
    vec![0x1e, 0x1c, op, 0xad]
}
/// `fload_0; fload_1; <op>; freturn`
fn float_binop(op: u8) -> Vec<u8> {
    vec![0x22, 0x23, op, 0xae]
}
/// `dload_0; dload_2; <op>; dreturn`
fn double_binop(op: u8) -> Vec<u8> {
    vec![0x26, 0x28, op, 0xaf]
}
/// `iload_0; <op>; ireturn`
fn int_unop(op: u8) -> Vec<u8> {
    vec![0x1a, op, 0xac]
}
/// A conversion chain that starts from an int local and ends in `ireturn`.
fn conv_to_int(ops: &[u8]) -> Vec<u8> {
    let mut v = vec![0x1a];
    v.extend_from_slice(ops);
    v.push(0xac);
    v
}

/// Counted loop `for (i = 0; i < n; i++) acc += <body>` returning acc.
/// locals: 0 = n (param), 1 = acc, 2 = i.
fn counted_loop(body: &[u8]) -> Vec<u8> {
    // 0: iconst_0 / 1: istore_1 / 2: iconst_0 / 3: istore_2
    let mut v = vec![0x03, 0x3c, 0x03, 0x3d];
    let head = v.len(); // 4
    v.extend_from_slice(&[0x1c, 0x1a]); // iload_2, iload_0
    let cmp_at = v.len();
    v.extend_from_slice(&[0xa2, 0x00, 0x00]); // if_icmpge <fix>
    v.extend_from_slice(&[0x1b]); // iload_1
    v.extend_from_slice(body);
    v.extend_from_slice(&[0x60, 0x3c]); // iadd, istore_1
    v.extend_from_slice(&[0x84, 0x02, 0x01]); // iinc 2 1
    let goto_at = v.len();
    let back = head as i32 - goto_at as i32;
    v.extend_from_slice(&[0xa7, (back >> 8) as u8, back as u8]); // goto head
    let exit = v.len();
    let fwd = exit as i32 - cmp_at as i32;
    v[cmp_at + 1] = (fwd >> 8) as u8;
    v[cmp_at + 2] = fwd as u8;
    v.extend_from_slice(&[0x1b, 0xac]); // iload_1, ireturn
    v
}

/// `tableswitch` over local 0 with `n` cases, all falling to the same return.
fn tableswitch(n: i32) -> Vec<u8> {
    let mut v = vec![0x1a]; // iload_0   (pc 0)
    v.push(0xaa); // tableswitch at pc 1
    while v.len() % 4 != 0 {
        v.push(0x00); // pad to 4-byte boundary from pc 0
    }
    // default / low / high, then n offsets.
    let table_start = v.len();
    let entry_len = 12 + 4 * n as usize;
    let after = table_start + entry_len; // pc of the first target
    let dflt = (after + 2) as i32 - 1; // relative to the tableswitch opcode pc (1)
    v.extend_from_slice(&dflt.to_be_bytes());
    v.extend_from_slice(&0i32.to_be_bytes());
    v.extend_from_slice(&(n - 1).to_be_bytes());
    for k in 0..n {
        let target = (after + 2 * k as usize) as i32 - 1;
        v.extend_from_slice(&target.to_be_bytes());
    }
    debug_assert_eq!(v.len(), after);
    for k in 0..n {
        v.push(0x03 + (k % 6) as u8); // iconst_<k mod 6>
        v.push(0xac); // ireturn
    }
    v.push(0x02); // iconst_m1
    v.push(0xac); // ireturn
    v
}

/// `lookupswitch` over local 0 with the given (key, const) pairs.
fn lookupswitch(pairs: &[(i32, u8)]) -> Vec<u8> {
    let mut v = vec![0x1a];
    v.push(0xab);
    while v.len() % 4 != 0 {
        v.push(0x00);
    }
    let table_start = v.len();
    let entry_len = 8 + 8 * pairs.len();
    let after = table_start + entry_len;
    let dflt = (after + 2 * pairs.len()) as i32 - 1;
    v.extend_from_slice(&dflt.to_be_bytes());
    v.extend_from_slice(&(pairs.len() as i32).to_be_bytes());
    for (i, (k, _)) in pairs.iter().enumerate() {
        v.extend_from_slice(&k.to_be_bytes());
        let target = (after + 2 * i) as i32 - 1;
        v.extend_from_slice(&target.to_be_bytes());
    }
    debug_assert_eq!(v.len(), after);
    for (_, c) in pairs {
        v.push(*c);
        v.push(0xac);
    }
    v.push(0x02);
    v.push(0xac);
    v
}

/// Counted loop accumulating into a **category-2** local, so the compile has a
/// `long`/`double` local with a dead high half AND a loop header where OSR
/// entry metadata is published. `store`/`load` are the cat-2 local 1 opcodes,
/// `conv` widens the int induction variable, `op` accumulates and `ret`
/// returns. Locals: 0 = n (param), 1+2 = accumulator, 3 = i.
fn cat2_accum_loop(zero: u8, store: u8, load: u8, conv: u8, op: u8, ret: u8) -> Vec<u8> {
    let mut v = vec![zero, store, 0x03, 0x3e]; // acc = 0; i = 0
    let head = v.len();
    v.extend_from_slice(&[0x1d, 0x1a]); // iload_3, iload_0
    let cmp_at = v.len();
    v.extend_from_slice(&[0xa2, 0x00, 0x00]); // if_icmpge <fix>
    v.extend_from_slice(&[load, 0x1d, conv, op, store]); // acc += (cat2) i
    v.extend_from_slice(&[0x84, 0x03, 0x01]); // iinc 3 1
    let goto_at = v.len();
    let back = head as i32 - goto_at as i32;
    v.extend_from_slice(&[0xa7, (back >> 8) as u8, back as u8]);
    let exit = v.len();
    let fwd = exit as i32 - cmp_at as i32;
    v[cmp_at + 1] = (fwd >> 8) as u8;
    v[cmp_at + 2] = fwd as u8;
    v.extend_from_slice(&[load, ret]);
    v
}

/// Two counted loops one after the other, each with its own `double`
/// accumulator in a *disjoint* live range. The graph-colouring allocator is
/// free to coalesce both onto one XMM register, which is exactly the state the
/// per-entry dead-local mask exists for: entering the second loop must not load
/// the first loop accumulator over the live owner of that register.
/// Locals: 0 = n, 1+2 = first accumulator, 3+4 = second, 5 = i.
fn two_disjoint_double_accumulators() -> Vec<u8> {
    fn one(v: &mut Vec<u8>, store: &[u8], load: &[u8]) {
        v.extend_from_slice(&[0x0e]); // dconst_0
        v.extend_from_slice(store);
        v.extend_from_slice(&[0x03, 0x36, 0x05]); // iconst_0; istore 5
        let head = v.len();
        v.extend_from_slice(&[0x15, 0x05, 0x1a]); // iload 5; iload_0
        let cmp_at = v.len();
        v.extend_from_slice(&[0xa2, 0x00, 0x00]);
        v.extend_from_slice(load);
        v.extend_from_slice(&[0x15, 0x05, 0x87, 0x63]); // iload 5; i2d; dadd
        v.extend_from_slice(store);
        v.extend_from_slice(&[0x84, 0x05, 0x01]); // iinc 5 1
        let goto_at = v.len();
        let back = head as i32 - goto_at as i32;
        v.extend_from_slice(&[0xa7, (back >> 8) as u8, back as u8]);
        let exit = v.len();
        let fwd = exit as i32 - cmp_at as i32;
        v[cmp_at + 1] = (fwd >> 8) as u8;
        v[cmp_at + 2] = fwd as u8;
    }
    let mut v = Vec::new();
    one(&mut v, &[0x48], &[0x27]); // dstore_1 / dload_1
    one(&mut v, &[0x39, 0x03], &[0x18, 0x03]); // dstore 3 / dload 3
    v.extend_from_slice(&[0x27, 0x18, 0x03, 0x63, 0xaf]); // d1 + d3; dreturn
    v
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

fn corpus() -> Vec<Case> {
    let mut c: Vec<Case> = Vec::new();

    // --- int arithmetic -----------------------------------------------------
    for (name, op) in [
        ("iadd", 0x60u8),
        ("isub", 0x64),
        ("imul", 0x68),
        ("idiv", 0x6c),
        ("irem", 0x70),
        ("ishl", 0x78),
        ("ishr", 0x7a),
        ("iushr", 0x7c),
        ("iand", 0x7e),
        ("ior", 0x80),
        ("ixor", 0x82),
    ] {
        c.push(Case::new(&format!("int/{name}"), 2, 2, int_binop(op)));
    }
    c.push(Case::new("int/ineg", 1, 1, int_unop(0x74)));

    // Constant-folding / peephole surface: `iload_0 <const> <op> ireturn`.
    for (name, konst, op) in [
        ("add_c7", vec![0x10u8, 0x07], 0x60u8),
        ("mul_c9", vec![0x10, 0x09], 0x68),
        ("mul_c16", vec![0x10, 0x10], 0x68),
        ("div_c8", vec![0x10, 0x08], 0x6c),
        ("div_c7", vec![0x10, 0x07], 0x6c),
        ("rem_c8", vec![0x10, 0x08], 0x70),
        ("rem_c7", vec![0x10, 0x07], 0x70),
        ("and_c255", vec![0x11, 0x00, 0xff], 0x7e),
        ("shl_c3", vec![0x06], 0x78),
        ("sub_c1", vec![0x04], 0x64),
    ] {
        let mut code = vec![0x1a];
        code.extend_from_slice(&konst);
        code.push(op);
        code.push(0xac);
        c.push(Case::new(&format!("peep/{name}"), 1, 1, code));
    }

    // --- long arithmetic ----------------------------------------------------
    for (name, op) in [
        ("ladd", 0x61u8),
        ("lsub", 0x65),
        ("lmul", 0x69),
        ("ldiv", 0x6d),
        ("lrem", 0x71),
        ("land", 0x7f),
        ("lor", 0x81),
        ("lxor", 0x83),
    ] {
        c.push(Case::new(&format!("long/{name}"), 4, 4, long_binop(op)));
    }
    for (name, op) in [("lshl", 0x79u8), ("lshr", 0x7b), ("lushr", 0x7d)] {
        c.push(Case::new(&format!("long/{name}"), 3, 3, long_shift(op)));
    }
    c.push(Case::new("long/lneg", 2, 2, vec![0x1e, 0x75, 0xad]));
    c.push(Case::new("long/lcmp", 4, 4, vec![0x1e, 0x20, 0x94, 0xac]));
    // long constant peepholes
    for (name, konst, op) in [
        ("ldiv_c8", vec![0x0au8], 0x6du8),
        ("lrem_c8", vec![0x0a], 0x71),
    ] {
        let mut code = vec![0x1e];
        code.extend_from_slice(&konst);
        code.push(op);
        code.push(0xad);
        c.push(Case::new(&format!("longpeep/{name}"), 2, 2, code));
    }

    // --- float / double -----------------------------------------------------
    for (name, op) in [
        ("fadd", 0x62u8),
        ("fsub", 0x66),
        ("fmul", 0x6a),
        ("fdiv", 0x6e),
    ] {
        c.push(Case::new(&format!("float/{name}"), 2, 2, float_binop(op)));
    }
    c.push(Case::new("float/fneg", 1, 1, vec![0x22, 0x76, 0xae]));
    c.push(Case::new("float/fcmpl", 2, 2, vec![0x22, 0x23, 0x95, 0xac]));
    c.push(Case::new("float/fcmpg", 2, 2, vec![0x22, 0x23, 0x96, 0xac]));
    for (name, op) in [
        ("dadd", 0x63u8),
        ("dsub", 0x67),
        ("dmul", 0x6b),
        ("ddiv", 0x6f),
    ] {
        c.push(Case::new(&format!("double/{name}"), 4, 4, double_binop(op)));
    }
    c.push(Case::new("double/dneg", 2, 2, vec![0x26, 0x77, 0xaf]));
    c.push(Case::new(
        "double/dcmpl",
        4,
        4,
        vec![0x26, 0x28, 0x97, 0xac],
    ));
    c.push(Case::new(
        "double/dcmpg",
        4,
        4,
        vec![0x26, 0x28, 0x98, 0xac],
    ));

    // --- conversions --------------------------------------------------------
    c.push(Case::new("conv/i2b", 1, 1, conv_to_int(&[0x91])));
    c.push(Case::new("conv/i2c", 1, 1, conv_to_int(&[0x92])));
    c.push(Case::new("conv/i2s", 1, 1, conv_to_int(&[0x93])));
    c.push(Case::new("conv/i2l2i", 1, 1, conv_to_int(&[0x85, 0x88])));
    c.push(Case::new("conv/i2f2i", 1, 1, conv_to_int(&[0x86, 0x8b])));
    c.push(Case::new("conv/i2d2i", 1, 1, conv_to_int(&[0x87, 0x8e])));
    c.push(Case::new("conv/i2l", 1, 2, vec![0x1a, 0x85, 0xad]));
    c.push(Case::new("conv/i2f", 1, 1, vec![0x1a, 0x86, 0xae]));
    c.push(Case::new("conv/i2d", 1, 2, vec![0x1a, 0x87, 0xaf]));
    c.push(Case::new("conv/l2i", 2, 2, vec![0x1e, 0x88, 0xac]));
    c.push(Case::new("conv/l2f", 2, 2, vec![0x1e, 0x89, 0xae]));
    c.push(Case::new("conv/l2d", 2, 2, vec![0x1e, 0x8a, 0xaf]));
    c.push(Case::new("conv/f2i", 1, 1, vec![0x22, 0x8b, 0xac]));
    c.push(Case::new("conv/f2l", 1, 2, vec![0x22, 0x8c, 0xad]));
    c.push(Case::new("conv/f2d", 1, 2, vec![0x22, 0x8d, 0xaf]));
    c.push(Case::new("conv/d2i", 2, 2, vec![0x26, 0x8e, 0xac]));
    c.push(Case::new("conv/d2l", 2, 2, vec![0x26, 0x8f, 0xad]));
    c.push(Case::new("conv/d2f", 2, 2, vec![0x26, 0x90, 0xae]));

    // --- constants ----------------------------------------------------------
    for (name, body) in [
        ("iconst_m1", vec![0x02u8, 0xac]),
        ("iconst_5", vec![0x08, 0xac]),
        ("bipush", vec![0x10, 0x7f, 0xac]),
        ("sipush", vec![0x11, 0x7f, 0xff, 0xac]),
        ("lconst_1", vec![0x0a, 0xad]),
        ("fconst_2", vec![0x0d, 0xae]),
        ("dconst_1", vec![0x0f, 0xaf]),
    ] {
        c.push(Case::new(&format!("const/{name}"), 0, 2, body));
    }
    c.push(
        Case::new("const/ldc_int", 0, 2, vec![0x12, 0x07, 0xac]).ldcs(vec![(0, 0x1234_5678i64)]),
    );
    c.push(
        Case::new("const/ldc2w_long", 0, 2, vec![0x14, 0x00, 0x07, 0xad])
            .ldc2ws(vec![(0, 0x0123_4567_89ab_cdefi64)]),
    );

    // --- stack shuffles -----------------------------------------------------
    c.push(Case::new(
        "stack/dup_add",
        1,
        1,
        vec![0x1a, 0x59, 0x60, 0xac],
    ));
    c.push(Case::new(
        "stack/dup_x1",
        2,
        2,
        vec![0x1a, 0x1b, 0x5a, 0x60, 0x64, 0xac],
    ));
    c.push(Case::new(
        "stack/swap",
        2,
        2,
        vec![0x1a, 0x1b, 0x5f, 0x64, 0xac],
    ));
    c.push(Case::new("stack/pop", 2, 2, vec![0x1a, 0x1b, 0x57, 0xac]));
    c.push(Case::new(
        "stack/dup2_cat2",
        2,
        4,
        vec![0x1e, 0x5c, 0x61, 0xad],
    ));
    c.push(Case::new(
        "stack/pop2_cat1",
        2,
        2,
        vec![0x1a, 0x1b, 0x58, 0x03, 0xac],
    ));
    // FORM-2: a single category-2 value discarded. This is the shape javac
    // emits for a `long`/`double`-returning call used as a statement, and the
    // one that kept commons-math's P-square hot loop interpreted while
    // `pop2` was unimplemented — the cat-1 case above recorded REFUSED for just
    // as long, but a differ only reports, it does not fail.
    // lload_0 / pop2 / iconst_0 / ireturn
    c.push(Case::new(
        "stack/pop2_cat2",
        2,
        2,
        vec![0x1e, 0x58, 0x03, 0xac],
    ));
    // dup2_x1 FORM-2, the `return this.doubleField = value;` shape, minus the
    // putfield so the case needs no constant pool:
    // aload_0 / dload_1 / dup2_x1 / dreturn
    // Returning with operands still on the stack is legal and keeps the case to
    // the one opcode under test — an earlier draft ended in `pop / pop2`, and
    // it was the trailing `pop2` (whose width oracle cannot classify a `pop`)
    // that recorded REFUSED, not the dup2_x1 the case is named for.
    c.push(Case::new(
        "stack/dup2_x1_cat2",
        3,
        4,
        vec![0x2a, 0x18, 0x01, 0x5d, 0xaf],
    ));
    // The chained-assignment shape `a = b = 0.0` — the second `dup2` follows a
    // STORE, which the width oracle can only classify via the dup-then-store
    // pair rule.
    // dconst_0 / dup2 / dstore_1 / dup2 / dstore_3 / dreturn
    c.push(Case::new(
        "stack/dup2_after_store",
        2,
        5,
        vec![0x0e, 0x5c, 0x48, 0x5c, 0x4a, 0xaf],
    ));
    c.push(Case::new(
        "stack/dup2_x1",
        3,
        3,
        vec![0x1a, 0x1b, 0x1c, 0x5d, 0x60, 0x60, 0x60, 0x60, 0xac],
    ));

    // --- branches -----------------------------------------------------------
    for (name, op) in [
        ("ifeq", 0x99u8),
        ("ifne", 0x9a),
        ("iflt", 0x9b),
        ("ifge", 0x9c),
        ("ifgt", 0x9d),
        ("ifle", 0x9e),
    ] {
        // iload_0 / <op> +7 / iconst_0 / ireturn / iconst_1 / ireturn
        let code = vec![0x1a, op, 0x00, 0x05, 0x03, 0xac, 0x04, 0xac];
        c.push(Case::new(&format!("branch/{name}"), 1, 1, code));
    }
    for (name, op) in [
        ("if_icmpeq", 0x9fu8),
        ("if_icmpne", 0xa0),
        ("if_icmplt", 0xa1),
        ("if_icmpge", 0xa2),
        ("if_icmpgt", 0xa3),
        ("if_icmple", 0xa4),
    ] {
        let code = vec![0x1a, 0x1b, op, 0x00, 0x05, 0x03, 0xac, 0x04, 0xac];
        c.push(Case::new(&format!("branch/{name}"), 2, 2, code));
    }
    // branch hints, both polarities, over the same shape
    c.push(
        Case::new(
            "branch/hinted_taken",
            1,
            1,
            vec![0x1a, 0x99, 0x00, 0x05, 0x03, 0xac, 0x04, 0xac],
        )
        .hints(vec![(1, true)]),
    );
    c.push(
        Case::new(
            "branch/hinted_nottaken",
            1,
            1,
            vec![0x1a, 0x99, 0x00, 0x05, 0x03, 0xac, 0x04, 0xac],
        )
        .hints(vec![(1, false)]),
    );
    // goto_w
    c.push(Case::new(
        "branch/goto_w",
        0,
        1,
        vec![0xc8, 0x00, 0x00, 0x00, 0x07, 0x03, 0xac, 0x04, 0xac],
    ));
    // cmov / min-max peephole shape: a < b ? a : b
    c.push(Case::new(
        "branch/minmax",
        2,
        2,
        vec![
            0x1a, 0x1b, 0xa1, 0x00, 0x07, 0x1b, 0xa7, 0x00, 0x04, 0x1a, 0xac,
        ],
    ));

    // --- switches -----------------------------------------------------------
    c.push(Case::new("switch/table4", 1, 1, tableswitch(4)));
    c.push(Case::new("switch/table16", 1, 1, tableswitch(16)));
    c.push(Case::new(
        "switch/lookup",
        1,
        1,
        lookupswitch(&[(1, 0x03), (7, 0x04), (99, 0x05), (1000, 0x06)]),
    ));

    // --- loops --------------------------------------------------------------
    c.push(Case::new("loop/sum_i", 1, 3, counted_loop(&[0x1c])));
    c.push(Case::new(
        "loop/sum_i_times_3",
        1,
        3,
        counted_loop(&[0x1c, 0x06, 0x68]),
    ));
    c.push(Case::new("loop/unroll_hint4", 1, 3, counted_loop(&[0x1c])).unroll(vec![(4, 4)]));
    c.push(Case::new("loop/unroll_hint8", 1, 3, counted_loop(&[0x1c])).unroll(vec![(4, 8)]));
    // nested loop
    {
        let inner = counted_loop(&[0x1c]);
        let mut v = vec![0x03, 0x3c, 0x03, 0x3d]; // acc=0, i=0
        let head = v.len();
        v.extend_from_slice(&[0x1c, 0x1a]);
        let cmp_at = v.len();
        v.extend_from_slice(&[0xa2, 0x00, 0x00]);
        // body: recompute inner sum, discard all but its return -> use iload_1 add
        v.extend_from_slice(&[0x1b]);
        v.extend_from_slice(&inner[..inner.len() - 2]); // drop trailing iload_1/ireturn
        v.extend_from_slice(&[0x1b, 0x60, 0x3c]);
        v.extend_from_slice(&[0x84, 0x02, 0x01]);
        let goto_at = v.len();
        let back = head as i32 - goto_at as i32;
        v.extend_from_slice(&[0xa7, (back >> 8) as u8, back as u8]);
        let exit = v.len();
        let fwd = exit as i32 - cmp_at as i32;
        v[cmp_at + 1] = (fwd >> 8) as u8;
        v[cmp_at + 2] = fwd as u8;
        v.extend_from_slice(&[0x1b, 0xac]);
        c.push(Case::new("loop/nested", 1, 4, v));
    }

    // --- category-2 accumulators in counted loops ---------------------------
    // These are the shapes that give a `long`/`double` local a register home at
    // a published OSR entry, i.e. the only shapes where the high-half nulling
    // and the per-entry dead-local mask can be observed at all. A corpus of int
    // loops leaves that whole path unmeasured.
    c.push(Case::new(
        "loop/double_accum",
        1,
        4,
        cat2_accum_loop(0x0e, 0x48, 0x27, 0x87, 0x63, 0xaf),
    ));
    c.push(Case::new(
        "loop/long_accum",
        1,
        4,
        cat2_accum_loop(0x09, 0x40, 0x1f, 0x85, 0x61, 0xad),
    ));
    c.push(Case::new("loop/float_accum", 1, 3, {
        let mut v = vec![0x0b, 0x3c, 0x03, 0x3d]; // f = 0; i = 0
        let head = v.len();
        v.extend_from_slice(&[0x1c, 0x1a]);
        let cmp_at = v.len();
        v.extend_from_slice(&[0xa2, 0x00, 0x00]);
        v.extend_from_slice(&[0x23, 0x1c, 0x86, 0x62, 0x3c]);
        v.extend_from_slice(&[0x84, 0x02, 0x01]);
        let goto_at = v.len();
        let back = head as i32 - goto_at as i32;
        v.extend_from_slice(&[0xa7, (back >> 8) as u8, back as u8]);
        let exit = v.len();
        let fwd = exit as i32 - cmp_at as i32;
        v[cmp_at + 1] = (fwd >> 8) as u8;
        v[cmp_at + 2] = fwd as u8;
        v.extend_from_slice(&[0x23, 0xae]);
        v
    }));
    c.push(Case::new(
        "loop/two_double_accums",
        1,
        6,
        two_disjoint_double_accumulators(),
    ));

    // --- arrays -------------------------------------------------------------
    // newarray T_INT(10), store, load, arraylength
    let arr = |atype: u8, store: u8, load: u8, ret: u8, konst: &[u8]| {
        let mut v = vec![0x10, 0x20, 0xbc, atype, 0x4b]; // bipush 32; newarray; astore_0
        v.extend_from_slice(&[0x2a, 0x05]); // aload_0, iconst_2
        v.extend_from_slice(konst);
        v.push(store);
        v.extend_from_slice(&[0x2a, 0x05, load, ret]);
        v
    };
    c.push(Case::new("array/int", 0, 3, arr(10, 0x4f, 0x2e, 0xac, &[0x07])).heap());
    c.push(Case::new("array/byte", 0, 3, arr(8, 0x54, 0x33, 0xac, &[0x07])).heap());
    c.push(Case::new("array/char", 0, 3, arr(5, 0x55, 0x34, 0xac, &[0x07])).heap());
    c.push(Case::new("array/short", 0, 3, arr(9, 0x56, 0x35, 0xac, &[0x07])).heap());
    c.push(Case::new("array/long", 0, 4, arr(11, 0x50, 0x2f, 0xad, &[0x0a])).heap());
    c.push(Case::new("array/float", 0, 3, arr(6, 0x51, 0x30, 0xae, &[0x0c])).heap());
    c.push(Case::new("array/double", 0, 4, arr(7, 0x52, 0x31, 0xaf, &[0x0f])).heap());
    c.push(
        Case::new(
            "array/arraylength",
            0,
            2,
            vec![0x10, 0x20, 0xbc, 0x0a, 0x4b, 0x2a, 0xbe, 0xac],
        )
        .heap(),
    );
    c.push(
        Case::new(
            "array/anewarray",
            0,
            2,
            vec![0x10, 0x08, 0xbd, 0x00, 0x07, 0x4b, 0x2a, 0xbe, 0xac],
        )
        .heap()
        .anewarrays(vec![(2, 42)]),
    );
    c.push(
        Case::new(
            "array/multianewarray",
            0,
            2,
            vec![0x05, 0x06, 0xc5, 0x00, 0x07, 0x02, 0x4b, 0x2a, 0xbe, 0xac],
        )
        .heap()
        .multis(vec![(2, cratonvm_jit::pack_multianewarray_site(1, 7))]),
    );
    // array sum loop (BCE / SIMD candidate)
    {
        let mut v = vec![0x03, 0x3c, 0x03, 0x3d]; // acc=0, i=0
        let head = v.len();
        v.extend_from_slice(&[0x1c, 0x2a, 0xbe]); // iload_2, aload_0, arraylength
        let cmp_at = v.len();
        v.extend_from_slice(&[0xa2, 0x00, 0x00]);
        v.extend_from_slice(&[0x1b, 0x2a, 0x1c, 0x2e, 0x60, 0x3c]); // acc += a[i]
        v.extend_from_slice(&[0x84, 0x02, 0x01]);
        let goto_at = v.len();
        let back = head as i32 - goto_at as i32;
        v.extend_from_slice(&[0xa7, (back >> 8) as u8, back as u8]);
        let exit = v.len();
        let fwd = exit as i32 - cmp_at as i32;
        v[cmp_at + 1] = (fwd >> 8) as u8;
        v[cmp_at + 2] = fwd as u8;
        v.extend_from_slice(&[0x1b, 0xac]);
        c.push(Case::new("array/sum_loop", 1, 3, v.clone()).heap());
        c.push(
            Case::new("array/sum_loop_unroll", 1, 3, v)
                .heap()
                .unroll(vec![(4, 4)]),
        );
    }
    // array fill loop (bulk-store preheader candidate)
    {
        let mut v = vec![0x03, 0x3d]; // i = 0  (local 2)
        let head = v.len();
        v.extend_from_slice(&[0x1c, 0x2a, 0xbe]);
        let cmp_at = v.len();
        v.extend_from_slice(&[0xa2, 0x00, 0x00]);
        v.extend_from_slice(&[0x2a, 0x1c, 0x03, 0x4f]); // a[i] = 0
        v.extend_from_slice(&[0x84, 0x02, 0x01]);
        let goto_at = v.len();
        let back = head as i32 - goto_at as i32;
        v.extend_from_slice(&[0xa7, (back >> 8) as u8, back as u8]);
        let exit = v.len();
        let fwd = exit as i32 - cmp_at as i32;
        v[cmp_at + 1] = (fwd >> 8) as u8;
        v[cmp_at + 2] = fwd as u8;
        v.extend_from_slice(&[0xb1]); // return
        c.push(Case::new("array/fill_loop", 1, 3, v).heap());
    }
    // ifnull / ifnonnull on an array ref
    c.push(
        Case::new(
            "array/ifnull",
            1,
            1,
            vec![0x2a, 0xc6, 0x00, 0x05, 0x03, 0xac, 0x04, 0xac],
        )
        .heap(),
    );
    c.push(
        Case::new(
            "array/ifnonnull",
            1,
            1,
            vec![0x2a, 0xc7, 0x00, 0x05, 0x03, 0xac, 0x04, 0xac],
        )
        .heap(),
    );
    c.push(
        Case::new(
            "array/aconst_null_acmp",
            1,
            1,
            vec![0x2a, 0x01, 0xa5, 0x00, 0x05, 0x03, 0xac, 0x04, 0xac],
        )
        .heap(),
    );

    // --- fields -------------------------------------------------------------
    for (name, tag, load, ret, locals) in [
        ("int", b'I', 0xb4u8, 0xacu8, 2usize),
        ("long", b'J', 0xb4, 0xad, 3),
        ("float", b'F', 0xb4, 0xae, 2),
        ("double", b'D', 0xb4, 0xaf, 3),
        ("ref", b'L', 0xb4, 0xb0, 2),
    ] {
        c.push(
            Case::new(
                &format!("field/get_{name}"),
                1,
                locals,
                vec![0x2a, load, 0x00, 0x07, ret],
            )
            .heap()
            .fields(vec![(1, 3, tag)]),
        );
    }
    // putfield of each width
    for (name, tag, store_src, locals) in [
        ("int", b'I', vec![0x1b_u8], 2usize),
        ("long", b'J', vec![0x1f], 3),
        ("float", b'F', vec![0x23], 2),
        ("double", b'D', vec![0x27], 3),
        ("ref", b'L', vec![0x2b], 2),
    ] {
        let mut code = vec![0x2a];
        code.extend_from_slice(&store_src);
        code.extend_from_slice(&[0xb5, 0x00, 0x07, 0xb1]);
        c.push(
            Case::new(&format!("field/put_{name}"), 2, locals, code)
                .heap()
                .fields(vec![(2, 3, tag)]),
        );
    }
    // getstatic / putstatic
    c.push(
        Case::new("field/getstatic_int", 0, 1, vec![0xb2, 0x00, 0x07, 0xac])
            .heap()
            .statics(vec![(0, 11, 3, b'I', false)]),
    );
    c.push(
        Case::new(
            "field/getstatic_volatile",
            0,
            1,
            vec![0xb2, 0x00, 0x07, 0xac],
        )
        .heap()
        .statics(vec![(0, 11, 3, b'I', true)]),
    );
    c.push(
        Case::new(
            "field/putstatic_int",
            1,
            1,
            vec![0x1a, 0xb3, 0x00, 0x07, 0xb1],
        )
        .heap()
        .statics(vec![(1, 11, 3, b'I', false)]),
    );
    c.push(
        Case::new(
            "field/putstatic_ref",
            1,
            1,
            vec![0x2a, 0xb3, 0x00, 0x07, 0xb1],
        )
        .heap()
        .statics(vec![(1, 11, 3, b'L', false)]),
    );
    // getfield inside a loop (LICM / null-check-elimination surface)
    {
        let mut v = vec![0x03, 0x3c, 0x03, 0x3d];
        let head = v.len();
        v.extend_from_slice(&[0x1c, 0x10, 0x40]);
        let cmp_at = v.len();
        v.extend_from_slice(&[0xa2, 0x00, 0x00]);
        v.extend_from_slice(&[0x1b, 0x2a, 0xb4, 0x00, 0x07, 0x60, 0x3c]);
        v.extend_from_slice(&[0x84, 0x02, 0x01]);
        let goto_at = v.len();
        let back = head as i32 - goto_at as i32;
        v.extend_from_slice(&[0xa7, (back >> 8) as u8, back as u8]);
        let exit = v.len();
        let fwd = exit as i32 - cmp_at as i32;
        v[cmp_at + 1] = (fwd >> 8) as u8;
        v[cmp_at + 2] = fwd as u8;
        v.extend_from_slice(&[0x1b, 0xac]);
        let gf = v.iter().position(|&b| b == 0xb4).unwrap();
        c.push(
            Case::new("field/get_in_loop", 1, 3, v)
                .heap()
                .fields(vec![(gf, 3, b'I')]),
        );
    }

    // --- allocation ---------------------------------------------------------
    c.push(
        Case::new("new/helper", 0, 2, vec![0xbb, 0x00, 0x07, 0x4b, 0x2a, 0xb0])
            .heap()
            .news(vec![(0, 55, 32, false, false)]),
    );
    c.push(
        Case::new(
            "new/inline_tlab",
            0,
            2,
            vec![0xbb, 0x00, 0x07, 0x4b, 0x2a, 0xb0],
        )
        .heap()
        .tlab()
        .news(vec![(0, 55, 32, true, false)]),
    );
    c.push(
        Case::new(
            "new/inline_tlab_flag2",
            0,
            2,
            vec![0xbb, 0x00, 0x07, 0x4b, 0x2a, 0xb0],
        )
        .heap()
        .tlab()
        .news(vec![(0, 55, 48, true, true)]),
    );

    // --- monitors, throw, misc ---------------------------------------------
    c.push(
        Case::new(
            "misc/monitor",
            1,
            1,
            vec![0x2a, 0xc2, 0x2a, 0xc3, 0x03, 0xac],
        )
        .heap(),
    );
    c.push(Case::new("misc/athrow", 1, 1, vec![0x2a, 0xbf]).heap());
    c.push(Case::new("misc/return_void", 0, 1, vec![0xb1]));
    c.push(Case::new("misc/areturn_param", 1, 1, vec![0x2a, 0xb0]).heap());
    c.push(Case::new(
        "misc/wide_iload",
        1,
        300,
        vec![0xc4, 0x15, 0x00, 0x00, 0xac],
    ));
    c.push(Case::new(
        "misc/wide_iinc",
        1,
        300,
        vec![0xc4, 0x84, 0x00, 0x00, 0x00, 0x05, 0x1a, 0xac],
    ));
    c.push(Case::new("misc/many_locals", 1, 64, {
        let mut v = vec![0x1a];
        for i in 1..40u8 {
            v.push(0x36); // istore <i>
            v.push(i);
            v.push(0x15); // iload <i>
            v.push(i);
        }
        v.push(0xac);
        v
    }));
    c.push(Case::new("misc/deep_stack", 1, 2, {
        let mut v = vec![0x1a];
        for _ in 0..24 {
            v.push(0x1a);
        }
        for _ in 0..24 {
            v.push(0x60);
        }
        v.push(0xac);
        v
    }));

    // --- long-parameter slot layout ----------------------------------------
    c.push(Case::new(
        "params/long_first",
        3,
        4,
        vec![0x1e, 0x1c, 0x85, 0x61, 0xad],
    ));
    c.push(Case::new(
        "params/mixed_fp",
        4,
        5,
        vec![0x26, 0x22, 0x8d, 0x63, 0xaf],
    ));

    c
}

/// The metadata hash is only worth having if the corpus reaches the metadata.
///
/// Without this, `metadata_hash` could be a constant across every case — every
/// OSR table `None`, every oop-map list empty — and the comparison would pass
/// through any change to the publication region while looking like coverage.
/// The exact edit that trips it: delete one of the loop or array cases from
/// `corpus()`, or make `compile` stop publishing an OSR entry table.
#[test]
fn corpus_actually_exercises_the_published_metadata() {
    let mut with_osr_entries = 0;
    let mut with_oop_maps = 0;
    let mut with_reg_locals = 0;
    let mut with_cat2_reg_home = 0;
    let mut distinct = std::collections::HashSet::new();

    for case in corpus() {
        let (status, _, _, meta) = case.run();
        if status != "OK" {
            continue;
        }
        distinct.insert(meta);
        let helpers = fake_helpers(case.inline_tlab);
        let cm = case.compile_with(&helpers).expect("recompiled");
        if cm
            .osr_pc_to_native
            .as_ref()
            .is_some_and(|t| t.iter().any(|&e| e >= 0))
        {
            with_osr_entries += 1;
        }
        if !cm.oop_maps.is_empty() {
            with_oop_maps += 1;
        }
        if cm.osr_num_reg_locals > 0 {
            with_reg_locals += 1;
        }
        // A category-2 local at index N has a dead high half at N+1. The OSR
        // metadata is required to leave that half unassigned in BOTH files.
        let osr_entered = cm
            .osr_pc_to_native
            .as_ref()
            .is_some_and(|t| t.iter().any(|&e| e >= 0));
        let cat2_homed = |a: &Option<Vec<Option<u8>>>| {
            a.as_ref().is_some_and(|v| {
                v.iter()
                    .enumerate()
                    .any(|(i, e)| e.is_some() && i + 1 < v.len() && v[i + 1].is_none())
            })
        };
        if osr_entered
            && (cat2_homed(&cm.osr_local_assignments) || cat2_homed(&cm.osr_xmm_assignments))
        {
            with_cat2_reg_home += 1;
        }
    }

    assert!(
        with_osr_entries >= 5,
        "only {with_osr_entries} corpus cases publish a reachable OSR entry; the \
         metadata column cannot detect damage to a table nothing builds"
    );
    assert!(
        with_oop_maps >= 1,
        "no corpus case publishes an oop map: the GC half of the fingerprint is \
         vacuous"
    );
    assert!(
        with_cat2_reg_home >= 1,
        "no corpus case gives a category-2 local a register home at a published \
         OSR entry. That is the state the high-half nulling and the dead-local \
         mask exist for, and without it deleting either is invisible — which is \
         exactly what a mutation test found before `loop/double_accum` and its \
         siblings were added."
    );
    assert!(
        with_reg_locals >= 5,
        "only {with_reg_locals} cases give a local a register home, so the OSR \
         assignment vectors are almost all empty"
    );
    assert!(
        distinct.len() >= 40,
        "only {} distinct metadata hashes across the corpus — the column is \
         close to constant and would not localise a regression",
        distinct.len()
    );
}

#[test]
fn dump_corpus() {
    let cases = corpus();
    println!("CORPUS-BEGIN cases={}", cases.len());
    for case in &cases {
        let (status, len, hash, meta) = case.run();
        println!(
            "CORPUS\t{}\t{}\t{}\t{:016x}\t{:016x}",
            case.name, status, len, hash, meta
        );
    }
    println!("CORPUS-END");
}

/// Same corpus, compiled twice in one process: catches any nondeterminism in
/// the backend itself (hash-map iteration order, address-dependent decisions)
/// that would otherwise be misread as a regression from the split.
#[test]
fn corpus_is_deterministic_within_a_process() {
    let cases = corpus();
    for case in &cases {
        let a = case.run();
        let b = case.run();
        assert_eq!(a, b, "nondeterministic codegen for {}", case.name);
    }
}
