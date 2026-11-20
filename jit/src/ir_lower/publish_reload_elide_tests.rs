// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT rx27 wave 9, lane regalloc (`CRATONVM_JIT_IR_PUBLISH_RELOAD_ELIDE`):
//! A read immediately behind its own register or home publish is omitted.
//! Native leaves pin elimination, fallback, and the GC-visible home; arithmetic
//! loops retain their reference answers.

use super::edge_loop_fixture::{lower_int_method, run};


/// `REX.W 89 /r` register-to-register moves as `(dst, src)`, in order, with
/// their offsets (the encoding `emit_mov_reg_reg64_raw` writes: `reg` is the
/// source, `r/m` the destination).
fn reg_movs(code: &[u8]) -> Vec<(usize, u8, u8)> {
    (2..code.len())
        .filter(|&p| code[p - 2] & 0xFA == 0x48 && code[p - 1] == 0x89 && code[p] & 0xC0 == 0xC0)
        .map(|p| {
            let rex = code[p - 2];
            let src = ((code[p] >> 3) & 7) | (((rex >> 2) & 1) << 3);
            let dst = (code[p] & 7) | ((rex & 1) << 3);
            (p - 2, dst, src)
        })
        .collect()
}

/// Adjacent `MOV reg, RAX` / `MOV RAX, reg` pairs: a publish and its
/// immediate read back.
fn publish_reload_pairs(code: &[u8]) -> usize {
    reg_movs(code)
        .windows(2)
        .filter(|w| {
            let ((a, d0, s0), (b, d1, s1)) = (w[0], w[1]);
            b == a + 3 && s0 == 0 && d0 != 0 && d1 == 0 && s1 == d0
        })
        .count()
}

/// `static int fa(int n)`, from `javac`: `t = i * 5 + n` is read twice, the
/// second time right behind its definition.
const FA: [u8; 33] = [
    0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x17, 0x1c, 0x08, 0x68, 0x1a, 0x60, 0x3e, 0x1b,
    0x1d, 0x1d, 0x68, 0x1d, 0x60, 0x60, 0x3c, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xea, 0x1b, 0xac, 0, 0,
];
const FA_LEN: usize = 31;

fn fa_ref(n: i32) -> i32 {
    let mut s = 0i32;
    for i in 0..n {
        let t = i.wrapping_mul(5).wrapping_add(n);
        s = s.wrapping_add(t.wrapping_mul(t).wrapping_add(t));
    }
    s
}

/// `static int fc(int n)`, from `javac`: `t = i * 7 + 3` and `w = t ^ u` are
/// each read twice, the second time away from the definition.
const FC: [u8; 50] = [
    0x03, 0x3c, 0x04, 0x3d, 0x03, 0x3e, 0x1d, 0x1a, 0xa2, 0x00, 0x24, 0x1d, 0x10, 0x07, 0x68, 0x06,
    0x60, 0x36, 0x04, 0x15, 0x04, 0x1c, 0x82, 0x36, 0x05, 0x1b, 0x15, 0x05, 0x15, 0x04, 0x64, 0x60,
    0x3c, 0x1c, 0x15, 0x05, 0x60, 0x3d, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xdd, 0x1b, 0x1c, 0x60, 0xac,
    0, 0,
];
const FC_LEN: usize = 48;

fn fc_ref(n: i32) -> i32 {
    let (mut s, mut u) = (0i32, 1i32);
    for i in 0..n {
        let t = i.wrapping_mul(7).wrapping_add(3);
        let w = t ^ u;
        s = s.wrapping_add(w.wrapping_sub(t));
        u = u.wrapping_add(w);
    }
    s.wrapping_add(u)
}


/// The scanner on hand-written bytes.
#[test]
fn rx27w9_the_scanner() {
    // mov rbx,rax; mov rax,rbx; mov r12,rax; mov [rbp-8],rax; mov rax,r12; mov rcx,rax
    let code = [
        0x48, 0x89, 0xC3, 0x48, 0x89, 0xD8, 0x49, 0x89, 0xC4, 0x48, 0x89, 0x45, 0xF8, 0x4C, 0x89,
        0xE0, 0x48, 0x89, 0xC1,
    ];
    let movs = reg_movs(&code);
    assert_eq!(
        movs.iter().map(|&(_, d, s)| (d, s)).collect::<Vec<_>>(),
        vec![(3, 0), (0, 3), (12, 0), (0, 12), (1, 0)]
    );
    assert_eq!(
        publish_reload_pairs(&code),
        1,
        "the r12 pair has a store between"
    );
}

/// The 2026-10-29 regalloc flip: the four switches are default ON, `=0`
/// kills each one, and `=1` keeps it on.
fn unresident_reference_home() -> (super::Lowerer<'static>, super::NodeId, i32) {
    use super::*;
    let mut builder = crate::ir::IrBuilder::new(1, 1);
    builder.set_param_types(&[IrType::Ref]);
    let graph = Box::leak(Box::new(
        builder.build(&[0x2a, 0xb0], 2).expect("reference IR"),
    ));
    let schedule = Box::leak(Box::new(crate::ir_schedule::schedule(graph)));
    let plan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
    let id = graph
        .nodes
        .iter()
        .position(|n| matches!(n.op, Op::Param(0)))
        .expect("parameter") as NodeId;
    let mut lo = super::tests::lowerer_over(graph, schedule, plan, 4096);
    lo.gp_reg_of = vec![None; graph.nodes.len()];
    lo.gp_reg_live = vec![false; graph.nodes.len()];
    lo.home_reload_forward_on = true;
    lo.publish_reload_elide_on = true;
    lo.publish_reload_elided = vec![false; graph.nodes.len()];
    lo.publish_reload_excluded = vec![false; graph.nodes.len()];
    lo.cur_def = Some(id);
    let slot = lo.alloc_slot(id);
    (lo, id, slot)
}

#[test]
fn adjacent_reference_reloads_keep_their_home_and_omit_the_read() {
    use super::*;
    for resident in [false, true] {
        for enabled in [false, true] {
            for excluded in [false, true] {
                for clobber in [false, true] {
                    for bits in [0_u64, 1, 0x1234_5678_abcdef00, u64::MAX] {
                        let (mut lo, id, slot) = unresident_reference_home();
                        lo.publish_reload_elide_on = enabled;
                        lo.publish_at_def_on = true;
                        lo.ref_residency_on = resident;
                        if resident {
                            lo.gp_reg_of[id as usize] = Some(3);
                        }
                        lo.publish_reload_excluded[id as usize] = excluded;
                        lo.buf.emit(&[0x55, 0x53]); // preserve RBP/RBX
                        lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]);
                        lo.emit_mov_reg_imm64(RAX, bits);
                        lo.store_rax(slot);
                        if clobber {
                            lo.emit_mov_reg_imm64(RAX, 0xBAD);
                        }
                        let before = lo.buf.pos();
                        lo.gp_load_value(RAX, id);
                        let skipped = enabled && !excluded && !clobber;
                        assert_eq!(
                            lo.buf.pos() == before,
                            skipped,
                            "resident={resident} enabled={enabled} excluded={excluded} clobber={clobber}"
                        );
                        assert_eq!(lo.publish_reload_elides.len(), usize::from(skipped));
                        lo.buf.emit(&[0x5b, 0x5d, 0xc3]);
                        lo.verify_publish_reload_elides();
                        assert!(lo.take_latched_bailout().is_none());
                        lo.buf.finalize();
                        let mut frame = vec![0x55AA_55AA_55AA_55AA_u64; slot as usize / 8 + 16];
                        let word = frame.len() - slot as usize / 8;
                        let mut expected = frame.clone();
                        expected[word] = bits;
                        // SAFETY: this leaf preserves RBP/RBX, writes only its valid
                        // supplied frame home, and never dereferences the reference.
                        let f: unsafe extern "C" fn(*mut u64) -> u64 =
                            unsafe { std::mem::transmute(lo.buf.as_ptr()) };
                        // SAFETY: this offset stays within the live fixture buffer used as the leaf frame.
                        let got = unsafe { f(frame.as_mut_ptr().add(frame.len())) };
                        assert_eq!(got, bits);
                        assert_eq!(frame, expected, "the GC-visible home remains written");
                    }
                }
            }
        }
    }
}

#[test]
fn a_branch_into_an_omitted_unresident_reference_reload_requires_a_retry() {
    use super::*;
    let (mut lo, id, slot) = unresident_reference_home();
    let branch = lo.emit_jmp_rel32();
    lo.emit_mov_reg_imm64(RAX, 123);
    lo.store_rax(slot);
    let read = lo.buf.pos();
    lo.gp_load_value(RAX, id);
    assert_eq!(lo.publish_reload_elides, vec![(read, id)]);
    lo.buf.emit_byte(0xc3);
    lo.patch_rel32_to_target(branch, read);
    lo.verify_publish_reload_elides();
    assert!(
        lo.take_latched_bailout().is_some(),
        "the branch bypasses the store that proves RAX"
    );
    assert_eq!(lo.publish_reload_refused, 1);
}

/// The final retry must not reintroduce a skipped read after the per-value
/// retry scope is gone. Exercise the production attempt with both policies.
#[test]
fn a_data_immediate_is_not_a_branch_into_a_skipped_reload() {
    use super::*;
    let (mut lo, id, slot) = unresident_reference_home();
    lo.buf.emit_byte(0x55); // preserve RBP
    lo.emit_mov_reg_reg64(5, ENTRY_ABI_REGS[0]);
    lo.buf.emit_byte(0xb9); // MOV ECX,imm32: data, not a relative address
    let immediate = lo.buf.pos();
    lo.buf.emit(&[0, 0, 0, 0]);
    let value = 0x1234_5678_abcd_ef00_u64;
    lo.emit_mov_reg_imm64(RAX, value);
    lo.store_rax(slot);
    let read = lo.buf.pos();
    lo.gp_load_value(RAX, id);
    assert_eq!(lo.publish_reload_elides, vec![(read, id)]);
    lo.buf.emit(&[0x5d, 0xc3]);
    // As a rel32 this ordinary integer would point at the omitted read.
    lo.buf
        .try_patch_i32(immediate, (read - immediate - 4) as i32)
        .unwrap();
    lo.verify_publish_reload_elides();
    let refusal = lo.take_latched_bailout();
    lo.buf.finalize();
    let mut frame = vec![0_u64; slot as usize / 8 + 16];
    let word = frame.len() - slot as usize / 8;
    // SAFETY: the leaf preserves RBP, writes only the supplied frame home,
    // and returns the value without dereferencing it or calling any helper.
    let f: unsafe extern "C" fn(*mut u64) -> u64 = unsafe { std::mem::transmute(lo.buf.as_ptr()) };
    // SAFETY: this offset stays within the live fixture buffer used as the leaf frame.
    assert_eq!(unsafe { f(frame.as_mut_ptr().add(frame.len())) }, value);
    assert_eq!(
        frame[word], value,
        "the authoritative home is still written"
    );
    assert!(
        refusal.is_none(),
        "a data immediate must not refuse an omitted reload: {refusal:?}"
    );
}
