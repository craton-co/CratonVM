use super::*;
use crate::x64::{R11, R12, R13, R15, R8, R9, RAX, RBP, RBX, RCX, RDI, RDX, RSI, RSP};

impl Tile {
    /// Build a tile directly. Tests only.
    ///
    /// In production only the rules construct tiles, so a tile's cover list
    /// and its instructions always come from one place. A test that needs to
    /// hand a *deliberately inconsistent* tile to a consumer — `ir_lower::
    /// mir_tile_is_emittable`'s absorbed-node guard is the one that does —
    /// cannot obtain one from a rule by construction, which is exactly why
    /// that guard needs this.
    ///
    /// Defined here, not beside `Tile::generic`: a test gate above
    /// production code ends the panic-free ratchet's scan of the file.
    pub fn for_test(root: NodeId, covered: Vec<NodeId>, insts: Vec<MInst>, rule: Rule) -> Tile {
        Tile::new(root, covered, insts, rule)
    }
}

// ── reference-emitter harness ─────────────────────────────────────────

/// A live `Compiler` whose hand-written emitters are the ground truth.
///
/// Every equivalence test drives the real emitter and the pattern table
/// with the same operands and compares the bytes. Nothing here
/// reimplements an encoding: the reference side is `x64.rs` itself.
struct Reference {
    compiler: super::super::Compiler,
}

impl Reference {
    fn new() -> Reference {
        let alloc = crate::regalloc::RegAllocResult {
            assignments: Vec::new(),
            xmm_assignments: Vec::new(),
            used_callee_saved: Vec::new(),
            used_xmm_regs: Vec::new(),
            block_live_in: Vec::new(),
        };
        let compiler = super::super::Compiler::new(
            "isel-equivalence".to_string(),
            crate::ExecutableBuffer::new(1 << 18).expect("test executable buffer"),
            0,
            0,
            1,
            false,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            alloc,
            false,
            cratonvm_jit_api::JitRuntimeHelpers::default(),
            0,
            false,
            false,
            false,
            false,
            // No `invokedynamic` in this fixture — see the sibling call
            // site in `x64/tests.rs`.
            false,
            Vec::new(),
        );
        Reference { compiler }
    }

    /// Run one hand-written emitter and return exactly the bytes it added.
    fn emit<F>(&mut self, f: F) -> Vec<u8>
    where
        F: FnOnce(&mut super::super::Compiler),
    {
        let start = self.compiler.buf.pos();
        // `emit_load_local` elides a reload when the previous emission
        // stored the same slot; clearing the mirror keeps each sample in
        // the sweep independent of its predecessor.
        self.compiler.slot_mirror = None;
        f(&mut self.compiler);
        assert!(
            !self.compiler.buf.overflowed(),
            "the reference emitter overflowed the test buffer"
        );
        self.compiler.buf.as_slice()[start..].to_vec()
    }
}

/// Register sweep: both halves of the file, both SIB special cases
/// (RSP/R12) and both no-`mod=00` special cases (RBP/R13).
const REGS: [u8; 13] = [
    RAX, RCX, RDX, RBX, RSP, RBP, RSI, RDI, R8, R11, R12, R13, R15,
];

/// Bases that a no-SIB encoding can legally address.
fn non_sib_regs() -> Vec<u8> {
    REGS.iter().copied().filter(|r| r & 7 != 0b100).collect()
}

fn sel(req: &Req) -> Vec<u8> {
    match select(req) {
        Ok(s) => s.encoded.bytes,
        Err(e) => panic!("no pattern for {req:?}: {e}"),
    }
}

fn named(name: &str, a: &Args) -> Vec<u8> {
    match encode_named(name, a) {
        Ok(e) => e.bytes,
        Err(e) => panic!("pattern `{name}` refused {a:?}: {e}"),
    }
}

fn gpr(op: Op, ty: Ty, dst: u8, src: u8) -> Req {
    Req::new(op, ty, Operand::Gpr(dst), Operand::Gpr(src))
}

fn gpr_imm(op: Op, ty: Ty, dst: u8, imm: i64) -> Req {
    Req::new(op, ty, Operand::Gpr(dst), Operand::Imm(imm))
}

/// A condition/opcode byte that satisfies the row's own constraints AND
/// cannot be mistaken for a REX prefix by the structural decoder — a
/// parametric opcode in `0x40..=0x4F` would decode as one.
fn canonical_op_byte(p: &Pattern) -> u8 {
    match p.op {
        Op::Jcc => 0x85,  // JNE rel32
        Op::Cmov => 0x45, // CMOVNE
        _ => 0x31,        // XOR r/m32, r32 — a real one-byte ALU opcode
    }
}

/// The smallest immediate each row's declared field and constraints admit.
fn canonical_imm(p: &Pattern) -> i64 {
    match p.imm {
        // The wide-move rows require a value that does NOT fit i32.
        ImmForm::Imm64 => i64::from(i32::MAX) + 1,
        ImmForm::ImmU8 => 1,
        _ => 0,
    }
}

/// Operands that satisfy every constraint the row declares, so a
/// table-wide sweep exercises each row instead of skipping it.
fn args_for(p: &Pattern, dst: u8, src: u8, base: u8, index: u8, disp: i64) -> Args {
    Args {
        dst,
        src: if p.constraints.contains(&Constraint::SameRegister) {
            dst
        } else {
            src
        },
        mem: if p.constraints.contains(&Constraint::IndexNotRsp) {
            Mem::base_index(base, index, 4, disp)
        } else if matches!(p.disp, DispPolicy::Force32) {
            Mem::base_disp32(base, disp)
        } else {
            Mem::base_disp(base, disp)
        },
        imm: canonical_imm(p),
        op_byte: canonical_op_byte(p),
    }
}

// ── table well-formedness ─────────────────────────────────────────────

#[test]
fn every_pattern_name_is_unique_and_names_its_emitter() {
    for (i, p) in PATTERNS.iter().enumerate() {
        assert!(!p.name.is_empty(), "row {i} has no name");
        assert!(
            !p.emitter.is_empty(),
            "row `{}` names no reference emitter; every row must be \
             anchored to the hand-written code it reproduces",
            p.name
        );
        for q in PATTERNS.iter().skip(i + 1) {
            assert_ne!(p.name, q.name, "duplicate pattern name `{}`", p.name);
        }
    }
}

/// Schema invariants that, if broken, silently change what a row encodes.
#[test]
fn encoding_templates_are_internally_consistent() {
    for p in PATTERNS {
        if let RegF::Ext(n) = p.enc.reg {
            assert!(n < 8, "`{}`: opcode extension /{n} is not 0..=7", p.name);
        }
        if p.enc.rex_w {
            assert_eq!(
                p.enc.rex,
                RexMode::Always,
                "`{}`: REX.W is part of the encoding, so the REX byte cannot be optional",
                p.name
            );
        }
        if matches!(p.enc.rex, RexMode::Never) {
            assert!(
                !p.enc.rex_w,
                "`{}`: cannot suppress REX and request REX.W",
                p.name
            );
        }
        assert_eq!(
            p.is_memory(),
            !matches!(p.disp, DispPolicy::NotApplicable),
            "`{}`: a displacement policy and a memory operand must agree",
            p.name
        );
        if p.is_memory() {
            assert!(
                matches!(p.dst, OpKind::Mem) || matches!(p.src, OpKind::Mem),
                "`{}`: addresses memory but declares no memory operand",
                p.name
            );
        }
        if !p.enc.modrm {
            assert_eq!(
                p.enc.rm,
                RmF::None,
                "`{}`: no ModRM byte, so no r/m field",
                p.name
            );
        }
    }
}

/// `PlusReg` folds the register into the opcode's low three bits, so
/// those bits must be clear in the base byte or the fold corrupts it.
#[test]
fn plus_reg_opcodes_have_room_for_the_register() {
    for p in PATTERNS {
        if let Opcode::PlusReg(b) = p.enc.opcode {
            assert_eq!(
                b & 7,
                0,
                "`{}`: opcode {b:#04X} has no register room",
                p.name
            );
            for r in 0u8..8 {
                assert_eq!(b | r, b + r, "`{}`: fold must be an OR", p.name);
            }
        }
    }
}

/// `cost.bytes` is a floor, not a guess: nothing may encode shorter.
///
/// This also proves every row can encode *something*: a row whose
/// constraints contradict its own encoding template would fail here
/// rather than sit in the table looking authoritative.
#[test]
fn declared_cost_is_a_lower_bound_on_the_encoded_length() {
    for p in PATTERNS {
        // Zero displacement, low registers: the shortest operands any
        // row admits.
        let a = args_for(p, RAX, RCX, RAX, RCX, 0);
        let e = match p.encode(&a) {
            Ok(e) => e,
            Err(err) => {
                panic!(
                    "`{}` could not encode its canonical operands: {err}",
                    p.name
                )
            }
        };
        assert!(
            e.bytes.len() >= usize::from(p.cost.bytes),
            "`{}`: cost says {} bytes but it encoded {}",
            p.name,
            p.cost.bytes,
            e.bytes.len()
        );
    }
}

// ── byte-for-byte equivalence with x64.rs ─────────────────────────────

#[test]
fn mov_r64_r64_matches_emit_mov_reg_reg() {
    let mut r = Reference::new();
    for &dst in REGS.iter() {
        for &src in REGS.iter() {
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_mov_reg_reg(dst, src));
            let got = sel(&gpr(Op::Mov, Ty::I64, dst, src));
            assert_eq!(got, want, "MOV r{dst}, r{src}");
        }
    }
}

#[test]
fn mov_r64_r64_store_form_matches_emit_mov_r64_r64() {
    let mut r = Reference::new();
    for &dst in REGS.iter() {
        for &src in REGS.iter() {
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_mov_r64_r64(dst, src));
            let a = Args {
                dst,
                src,
                ..Args::default()
            };
            assert_eq!(
                named("mov_r64_r64_store_form", &a),
                want,
                "MOV r/m64 r{dst}, r{src}"
            );
        }
    }
    // `emit_mov_rbp_rsp` is that same row with dst=RBP, src=RSP.
    let want = r.emit(|c: &mut super::super::Compiler| c.emit_mov_rbp_rsp());
    let a = Args {
        dst: RBP,
        src: RSP,
        ..Args::default()
    };
    assert_eq!(named("mov_r64_r64_store_form", &a), want, "MOV RBP, RSP");
    assert_eq!(want, vec![0x48, 0x89, 0xE5]);
}

#[test]
fn mov_immediate_family_matches_the_shrinking_emitters() {
    let mut r = Reference::new();
    let values: [i64; 14] = [
        0,
        1,
        -1,
        127,
        -128,
        128,
        i64::from(i32::MAX),
        i64::from(i32::MIN),
        i64::from(i32::MAX) + 1,
        i64::from(i32::MIN) - 1,
        i64::from(u32::MAX),
        i64::from(u32::MAX) + 1,
        i64::MAX,
        i64::MIN,
    ];
    for &reg in REGS.iter() {
        for &v in values.iter() {
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_mov_imm64(reg, v));
            let got = sel(&gpr_imm(Op::Mov, Ty::I64, reg, v));
            assert_eq!(got, want, "MOV r{reg}, {v}");
        }
        // The i32-only entry point must agree over its own domain.
        for &v in values.iter() {
            if let Ok(v32) = i32::try_from(v) {
                let want =
                    r.emit(|c: &mut super::super::Compiler| c.emit_mov_imm32_sx(reg, v32));
                let got = sel(&gpr_imm(Op::Mov, Ty::I64, reg, v));
                assert_eq!(got, want, "MOV r{reg}, {v32} (imm32 entry point)");
            }
        }
        // …and the non-shrinking form stays ten bytes for every value.
        for &v in values.iter() {
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_mov_imm64_full(reg, v));
            let a = Args {
                dst: reg,
                imm: v,
                ..Args::default()
            };
            let got = named("mov_r64_imm64_full", &a);
            assert_eq!(got, want, "MOV r{reg}, {v} (full)");
            assert_eq!(got.len(), 10, "the IC sites depend on the fixed length");
        }
    }
}

#[test]
fn xor_zeroing_matches_emit_xor_reg_self() {
    let mut r = Reference::new();
    for &reg in REGS.iter() {
        let want = r.emit(|c: &mut super::super::Compiler| c.emit_xor_reg_self(reg));
        let got = sel(&gpr_imm(Op::Mov, Ty::I64, reg, 0));
        assert_eq!(got, want, "XOR r{reg}, r{reg}");
    }
}

#[test]
fn frame_slot_loads_and_stores_match_the_local_emitters() {
    let mut r = Reference::new();
    // Depths chosen around the disp8/disp32 boundary in both directions:
    // `modrm_rbp_disp` encodes `-depth`, so depth 128 is the last disp8.
    let depths: [i32; 10] = [0, 8, 16, 120, 127, 128, 129, 1024, -8, -128];
    for &reg in REGS.iter() {
        for &depth in depths.iter() {
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_load_local(reg, depth));
            let mem = Mem::base_disp(RBP, -i64::from(depth));
            let got = sel(&Req::new(
                Op::Mov,
                Ty::I64,
                Operand::Gpr(reg),
                Operand::Mem(mem),
            ));
            assert_eq!(got, want, "MOV r{reg}, [rbp - {depth}]");

            let want = r.emit(|c: &mut super::super::Compiler| c.emit_store_local(depth, reg));
            let got = sel(&Req::new(
                Op::Mov,
                Ty::I64,
                Operand::Mem(mem),
                Operand::Gpr(reg),
            ));
            assert_eq!(got, want, "MOV [rbp - {depth}], r{reg}");

            let want =
                r.emit(|c: &mut super::super::Compiler| c.emit_lea_frame_slot(reg, depth));
            let got = sel(&Req::new(
                Op::Lea,
                Ty::I64,
                Operand::Gpr(reg),
                Operand::Mem(mem),
            ));
            assert_eq!(got, want, "LEA r{reg}, [rbp - {depth}]");
        }
        // Caller-arg loads are the same row with a POSITIVE displacement:
        // the shadow-space/stack-arg offsets that used to be narrowed with
        // `as u8`.
        for disp in [8i32, 16, 48, 127, 128, 256, 1024] {
            let want =
                r.emit(|c: &mut super::super::Compiler| c.emit_load_caller_arg(reg, disp));
            let mem = Mem::base_disp(RBP, i64::from(disp));
            let got = sel(&Req::new(
                Op::Mov,
                Ty::I64,
                Operand::Gpr(reg),
                Operand::Mem(mem),
            ));
            assert_eq!(got, want, "MOV r{reg}, [rbp + {disp}]");
        }
    }
}

/// The RSP-relative store is the SIB special case: same row, different
/// base.
#[test]
fn rsp_relative_store_matches_emit_mov_rsp_disp_from_reg() {
    let mut r = Reference::new();
    for &reg in REGS.iter() {
        for disp in [0i32, 8, 32, 127, 128, 4096, -8] {
            let want = r
                .emit(|c: &mut super::super::Compiler| c.emit_mov_rsp_disp_from_reg(disp, reg));
            let mem = Mem::base_disp(RSP, i64::from(disp));
            let got = sel(&Req::new(
                Op::Mov,
                Ty::I64,
                Operand::Mem(mem),
                Operand::Gpr(reg),
            ));
            assert_eq!(got, want, "MOV [rsp + {disp}], r{reg}");
            assert_eq!(got[2] & 7, 0b100, "r/m must say `SIB follows`");
            assert_eq!(got[3], 0x24, "index-free SIB byte");
        }
    }
}

#[test]
fn disp32_memory_family_matches_the_tlab_and_field_emitters() {
    let mut r = Reference::new();
    let disps: [i32; 7] = [0, 8, 127, 128, 4096, -4096, i32::MAX];
    for &dst in REGS.iter() {
        for &base in non_sib_regs().iter() {
            for &disp in disps.iter() {
                let mem = Mem::base_disp32(base, i64::from(disp));
                let rm =
                    |op: Op, ty: Ty| Req::new(op, ty, Operand::Gpr(dst), Operand::Mem(mem));

                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_mov_r64_mem_disp32(dst, base, disp)
                });
                assert_eq!(sel(&rm(Op::Mov, Ty::I64)), want, "MOV r64 disp32");

                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_movsxd_r64_mem_disp32(dst, base, disp)
                });
                assert_eq!(sel(&rm(Op::Movsxd, Ty::I32)), want, "MOVSXD disp32");

                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_mov_r32_mem_disp32(dst, base, disp)
                });
                assert_eq!(sel(&rm(Op::Mov, Ty::I32)), want, "MOV r32 disp32");

                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_lea_r64_mem_disp32(dst, base, disp)
                });
                assert_eq!(sel(&rm(Op::Lea, Ty::I64)), want, "LEA disp32");

                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_cmp_r64_mem_disp32(dst, base, disp)
                });
                assert_eq!(sel(&rm(Op::Cmp, Ty::I64)), want, "CMP disp32");

                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_mov_mem_disp32_r64(base, dst, disp)
                });
                let got = sel(&Req::new(
                    Op::Mov,
                    Ty::I64,
                    Operand::Mem(mem),
                    Operand::Gpr(dst),
                ));
                assert_eq!(got, want, "MOV [disp32], r64");

                for (bits, signed, op, ty) in [
                    (8u8, true, Op::Movsx, Ty::I8),
                    (8, false, Op::Movzx, Ty::I8),
                    (16, true, Op::Movsx, Ty::I16),
                    (16, false, Op::Movzx, Ty::I16),
                ] {
                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_movx_r64_mem_disp32(dst, base, disp, bits, signed)
                    });
                    let kind = if signed { "MOVSX" } else { "MOVZX" };
                    assert_eq!(sel(&rm(op, ty)), want, "{kind} {bits} disp32");
                }
            }
        }
    }
}

/// The `*_mem_disp32` emitters now agree with the table on the SIB bases
/// too. They used to emit `mod=10, r/m=100` with no SIB byte, so an R12
/// base (a LOCAL_REGS register) made the CPU read the first displacement
/// byte as a SIB; the equivalence sweep above skips RSP/R12 because the
/// emitters were wrong there. This is the half it skipped.
#[test]
fn disp32_memory_family_matches_the_table_on_sib_bases() {
    let mut r = Reference::new();
    for &base in [RSP, R12].iter() {
        for &dst in [RAX, RDX, R8, R11, R13].iter() {
            for disp in [0i32, 8, 128, -4096] {
                let mem = Mem::base_disp32(base, i64::from(disp));
                let rm =
                    |op: Op, ty: Ty| Req::new(op, ty, Operand::Gpr(dst), Operand::Mem(mem));

                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_mov_r64_mem_disp32(dst, base, disp)
                });
                assert_eq!(sel(&rm(Op::Mov, Ty::I64)), want, "MOV r64 [r{base}]");
                // ModRM r/m=100, then the index-free SIB 0x24.
                assert_eq!(want[3], 0x24, "SIB byte for r{base}");

                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_movsxd_r64_mem_disp32(dst, base, disp)
                });
                assert_eq!(sel(&rm(Op::Movsxd, Ty::I32)), want, "MOVSXD [r{base}]");

                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_mov_r32_mem_disp32(dst, base, disp)
                });
                assert_eq!(sel(&rm(Op::Mov, Ty::I32)), want, "MOV r32 [r{base}]");

                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_lea_r64_mem_disp32(dst, base, disp)
                });
                assert_eq!(sel(&rm(Op::Lea, Ty::I64)), want, "LEA [r{base}]");

                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_cmp_r64_mem_disp32(dst, base, disp)
                });
                assert_eq!(sel(&rm(Op::Cmp, Ty::I64)), want, "CMP [r{base}]");

                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_mov_mem_disp32_r64(base, dst, disp)
                });
                let got = sel(&Req::new(
                    Op::Mov,
                    Ty::I64,
                    Operand::Mem(mem),
                    Operand::Gpr(dst),
                ));
                assert_eq!(got, want, "MOV [r{base}], r64");

                for (bits, signed, op, ty) in [
                    (8u8, true, Op::Movsx, Ty::I8),
                    (8, false, Op::Movzx, Ty::I8),
                    (16, true, Op::Movsx, Ty::I16),
                    (16, false, Op::Movzx, Ty::I16),
                ] {
                    let want = r.emit(|c: &mut super::super::Compiler| {
                        c.emit_movx_r64_mem_disp32(dst, base, disp, bits, signed)
                    });
                    assert_eq!(sel(&rm(op, ty)), want, "MOVX{bits} [r{base}]");                    }                }
            let want = r.emit(|c: &mut super::super::Compiler| {
                c.emit_mov_dword_mem_disp32_imm32(base, 64, -1)
            });
            let got = sel(&Req::new(
                Op::Mov,
                Ty::I32,
                Operand::Mem(Mem::base_disp32(base, 64)),
                Operand::Imm(-1),
            ));
            assert_eq!(got, want, "MOV DWORD [r{base} + 64], -1");
        }
    }
}

#[test]
fn dword_store_of_an_immediate_matches_emit_mov_dword_mem_disp32_imm32() {
    let mut r = Reference::new();
    for &base in non_sib_regs().iter() {
        for disp in [0i32, 64, 128, -4096] {
            for imm in [0i32, 1, -1, i32::MAX, i32::MIN] {
                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_mov_dword_mem_disp32_imm32(base, disp, imm)
                });
                let mem = Mem::base_disp32(base, i64::from(disp));
                let got = sel(&Req::new(
                    Op::Mov,
                    Ty::I32,
                    Operand::Mem(mem),
                    Operand::Imm(i64::from(imm)),
                ));
                assert_eq!(got, want, "MOV DWORD [r{base} + {disp}], {imm}");
            }
        }
    }
}

/// The stack-bang probe is the 32-bit load with an RSP base — the case
/// `emit_mov_r32_mem_disp32` declines and the generic row handles.
#[test]
fn stack_bang_probe_matches_emit_stack_bang_load() {
    let mut r = Reference::new();
    for disp in [-4096i32, -8192, -128, -1] {
        let want = r.emit(|c: &mut super::super::Compiler| c.emit_stack_bang_load(disp));
        let mem = Mem::base_disp32(RSP, i64::from(disp));
        let got = sel(&Req::new(
            Op::Mov,
            Ty::I32,
            Operand::Gpr(RAX),
            Operand::Mem(mem),
        ));
        assert_eq!(got, want, "MOV EAX, [rsp + {disp}]");
        assert_eq!(&got[..3], &[0x8B, 0x84, 0x24][..]);
    }
}

#[test]
fn byte_flag_test_matches_emit_test_mem8_imm8() {
    let mut r = Reference::new();
    for &base in non_sib_regs().iter() {
        for disp in [0i32, 8, 127, 128, 1024] {
            for imm in [0u8, 1, 0x80, 0xFF] {
                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_test_mem8_imm8(base, disp, imm)
                });
                let mem = Mem::base_disp(base, i64::from(disp));
                let got = sel(&Req::new(
                    Op::Test,
                    Ty::I8,
                    Operand::Mem(mem),
                    Operand::Imm(i64::from(imm)),
                ));
                assert_eq!(got, want, "TEST BYTE [r{base} + {disp}], {imm:#04X}");
            }
        }
    }
    // The zero-displacement safepoint poll keeps its explicit disp8 byte:
    // shrinking it to `mod=00` would move every following instruction.
    let mem = Mem::base_disp(RAX, 0);
    let got = sel(&Req::new(
        Op::Test,
        Ty::I8,
        Operand::Mem(mem),
        Operand::Imm(0x80),
    ));
    assert_eq!(got, vec![0xF6, 0x40, 0x00, 0x80]);
}

#[test]
fn indexed_byte_store_matches_emit_mov_mem8_indexed_imm8() {
    let mut r = Reference::new();
    // The emitter used to write a literal `mod=00` ModRM byte, which for
    // an RBP/R13 base means "no base, disp32 follows"; it now takes the
    // `mod=01` + `disp8(0)` form the row always produced, so every base is
    // in the sweep.
    for &base in REGS.iter() {
        for &index in REGS.iter() {
            if index == RSP {
                continue;
            }
            for value in [0u8, 1, 0xFF] {
                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_mov_mem8_indexed_imm8(base, index, value)
                });
                let mem = Mem::base_index(base, index, 1, 0);
                let got = sel(&Req::new(
                    Op::Mov,
                    Ty::I8,
                    Operand::Mem(mem),
                    Operand::Imm(i64::from(value)),
                ));
                assert_eq!(got, want, "MOV BYTE [r{base} + r{index}], {value}");
            }
        }
    }
}

#[test]
fn alu_and_compare_families_match_their_emitters() {
    let mut r = Reference::new();
    for &dst in REGS.iter() {
        for &src in REGS.iter() {
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_sub_r64_r64(dst, src));
            assert_eq!(sel(&gpr(Op::Sub, Ty::I64, dst, src)), want, "SUB r64");

            let want = r.emit(|c: &mut super::super::Compiler| c.emit_cmp_r64_r64(dst, src));
            assert_eq!(sel(&gpr(Op::Cmp, Ty::I64, dst, src)), want, "CMP r64");

            let want = r.emit(|c: &mut super::super::Compiler| c.emit_cmp_r32_r32(dst, src));
            assert_eq!(sel(&gpr(Op::Cmp, Ty::I32, dst, src)), want, "CMP r32");

            for opcode in [0x01u8, 0x29, 0x21, 0x09, 0x31, 0x39, 0x85] {
                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_alu_r32_r32(opcode, dst, src)
                });
                let got =
                    sel(&gpr(Op::Alu, Ty::I32, dst, src).with_extra(Operand::OpByte(opcode)));
                assert_eq!(got, want, "ALU {opcode:#04X} r32");
            }

            for cc in [0x44u8, 0x45, 0x4C, 0x4D, 0x4E, 0x4F] {
                let want = r.emit(|c: &mut super::super::Compiler| {
                    c.emit_cmov_cc_reg_reg(cc, dst, src)
                });
                let got = sel(&gpr(Op::Cmov, Ty::I64, dst, src).with_extra(Operand::Cc(cc)));
                assert_eq!(got, want, "CMOV{cc:#04X}");
            }
        }
        let want = r.emit(|c: &mut super::super::Compiler| c.emit_test_r64_r64(dst));
        assert_eq!(sel(&gpr(Op::Test, Ty::I64, dst, dst)), want, "TEST r64");

        let want = r.emit(|c: &mut super::super::Compiler| c.emit_test_r32_r32(dst));
        assert_eq!(sel(&gpr(Op::Test, Ty::I32, dst, dst)), want, "TEST r32");

        for imm in [0i8, 1, -1, 7, -8, i8::MAX, i8::MIN] {
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_add_r64_imm8(dst, imm));
            assert_eq!(
                sel(&gpr_imm(Op::Add, Ty::I64, dst, i64::from(imm))),
                want,
                "ADD r{dst}, {imm}"
            );

            let want = r.emit(|c: &mut super::super::Compiler| c.emit_and_r64_imm8(dst, imm));
            assert_eq!(
                sel(&gpr_imm(Op::And, Ty::I64, dst, i64::from(imm))),
                want,
                "AND r{dst}, {imm}"
            );

            let want = r.emit(|c: &mut super::super::Compiler| c.emit_or_r64_imm8(dst, imm));
            assert_eq!(
                sel(&gpr_imm(Op::Or, Ty::I64, dst, i64::from(imm))),
                want,
                "OR r{dst}, {imm}"
            );
        }

        for imm in [0i32, 1, -1, 0x8000_0000u32 as i32, i32::MAX] {
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_test_r64_imm32(dst, imm));
            assert_eq!(
                sel(&gpr_imm(Op::Test, Ty::I64, dst, i64::from(imm))),
                want,
                "TEST r{dst}, {imm}"
            );
        }

        for shift in [0u8, 1, 3, 32, 63] {
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_shr_r64_imm8(dst, shift));
            assert_eq!(
                sel(&gpr_imm(Op::Shr, Ty::I64, dst, i64::from(shift))),
                want,
                "SHR r{dst}, {shift}"
            );
        }
    }
}

/// The frame-adjust emitters, over the domain their call sites use.
///
/// `emit_prologue`, `emit_epilogue` and the stack-arg block all pass a
/// non-negative byte count; `stack_arg_block_size` cannot return a
/// negative one. See `known_divergence_*` below for what happens outside
/// that domain.
#[test]
fn frame_adjust_matches_emit_sub_add_rsp_imm_over_their_call_domain() {
    let mut r = Reference::new();
    for imm in [0i32, 8, 0x28, 64, 127, 128, 256, 4096, 1 << 20] {
        let want = r.emit(|c: &mut super::super::Compiler| c.emit_sub_rsp_imm(imm));
        assert_eq!(
            sel(&gpr_imm(Op::Sub, Ty::I64, RSP, i64::from(imm))),
            want,
            "SUB RSP, {imm}"
        );
        let want = r.emit(|c: &mut super::super::Compiler| c.emit_add_rsp_imm(imm));
        assert_eq!(
            sel(&gpr_imm(Op::Add, Ty::I64, RSP, i64::from(imm))),
            want,
            "ADD RSP, {imm}"
        );
    }
}

/// A NEGATIVE frame adjustment is the one place the table and the
/// hand-written emitter disagree, and the disagreement is size-only.
///
/// `emit_sub_rsp_imm` tests `(0..=127).contains(&imm)`, so a negative
/// immediate takes its `imm32` branch even though `imm8` would express
/// it. The table picks the smallest legal form. No call site reaches
/// this — every caller passes a byte count — but the difference is
/// pinned here so a migration does not discover it in the field.
#[test]
fn known_divergence_negative_rsp_adjust_is_shorter_in_the_table() {
    let mut r = Reference::new();
    let want = r.emit(|c: &mut super::super::Compiler| c.emit_sub_rsp_imm(-8));
    let got = sel(&gpr_imm(Op::Sub, Ty::I64, RSP, -8));
    assert_eq!(want, vec![0x48, 0x81, 0xEC, 0xF8, 0xFF, 0xFF, 0xFF]);
    assert_eq!(got, vec![0x48, 0x83, 0xEC, 0xF8]);
    // Same instruction, same operand value; only the immediate width
    // differs, and both are sign-extended to 64 bits.
    let d_want = decode(&want, true).expect("decodable");
    let d_got = decode(&got, true).expect("decodable");
    assert_eq!(d_want.reg(), d_got.reg(), "same /5 SUB extension");
    assert_eq!(d_want.rm_reg(), d_got.rm_reg(), "same destination");
    assert_eq!(
        i32::from_le_bytes([d_want.imm[0], d_want.imm[1], d_want.imm[2], d_want.imm[3]]),
        i32::from(d_got.imm[0] as i8),
        "same immediate value"
    );
}

#[test]
fn stack_and_control_flow_match_their_emitters() {
    let mut r = Reference::new();
    let want = r.emit(|c: &mut super::super::Compiler| c.emit_push_rbp());
    let a = Args {
        dst: RBP,
        ..Args::default()
    };
    assert_eq!(named("push_r64", &a), want, "PUSH RBP");
    assert_eq!(want, vec![0x55]);

    let want = r.emit(|c: &mut super::super::Compiler| c.emit_pop_rbp());
    assert_eq!(named("pop_r64", &a), want, "POP RBP");
    assert_eq!(want, vec![0x5D]);

    let want = r.emit(|c: &mut super::super::Compiler| c.emit_ret());
    assert_eq!(
        sel(&Req::new(Op::Ret, Ty::Void, Operand::None, Operand::None)),
        want,
        "RET"
    );

    // Extended registers pick up REX.B in the PlusReg forms.
    let a = Args {
        dst: R12,
        ..Args::default()
    };
    assert_eq!(named("push_r64", &a), vec![0x41, 0x54], "PUSH R12");
    assert_eq!(named("pop_r64", &a), vec![0x41, 0x5C], "POP R12");
}

/// The `Jcc`/`JMP` rows must reproduce both the bytes AND the patch site
/// their emitters hand back to the caller.
#[test]
fn branch_patch_sites_match_the_emitters_returned_offsets() {
    let mut r = Reference::new();
    let mut want_patch = 0usize;
    let want = r.emit(|c: &mut super::super::Compiler| {
        let start = c.buf.pos();
        let p = c.emit_jmp_rel32_patch();
        want_patch = p - start;
    });
    let sel_jmp = select(&Req::new(
        Op::Jmp,
        Ty::Void,
        Operand::Rel32(0),
        Operand::None,
    ))
    .expect("jmp rel32");
    assert_eq!(sel_jmp.encoded.bytes, want, "JMP rel32");
    assert_eq!(sel_jmp.encoded.imm_offset, Some(want_patch));
    assert_eq!(want_patch, 1);

    for cc in [0x84u8, 0x85, 0x8C, 0x8F] {
        let mut want_patch = 0usize;
        let want = r.emit(|c: &mut super::super::Compiler| {
            let start = c.buf.pos();
            let p = c.emit_jcc_rel32_patch(cc);
            want_patch = p - start;
        });
        let s = select(
            &Req::new(Op::Jcc, Ty::Void, Operand::Rel32(0), Operand::None)
                .with_extra(Operand::Cc(cc)),
        )
        .expect("jcc rel32");
        assert_eq!(s.encoded.bytes, want, "Jcc {cc:#04X}");
        assert_eq!(s.encoded.imm_offset, Some(want_patch));
        assert_eq!(want_patch, 2);
    }
}

#[test]
fn xmm_family_matches_its_emitters() {
    let mut r = Reference::new();
    for &xmm in REGS.iter() {
        for &gp in REGS.iter() {
            let want =
                r.emit(|c: &mut super::super::Compiler| c.emit_movq_xmm_from_gpr(xmm, gp));
            let got = sel(&Req::new(
                Op::Movq,
                Ty::I64,
                Operand::Xmm(xmm),
                Operand::Gpr(gp),
            ));
            assert_eq!(got, want, "MOVQ xmm{xmm}, r{gp}");

            let want =
                r.emit(|c: &mut super::super::Compiler| c.emit_movq_gpr_from_xmm(gp, xmm));
            let got = sel(&Req::new(
                Op::Movq,
                Ty::I64,
                Operand::Gpr(gp),
                Operand::Xmm(xmm),
            ));
            assert_eq!(got, want, "MOVQ r{gp}, xmm{xmm}");
        }
        for &src in REGS.iter() {
            let want = r.emit(|c: &mut super::super::Compiler| c.emit_movsd_xmm_xmm(xmm, src));
            let got = sel(&Req::new(
                Op::Movsd,
                Ty::F64,
                Operand::Xmm(xmm),
                Operand::Xmm(src),
            ));
            assert_eq!(got, want, "MOVSD xmm{xmm}, xmm{src}");

            let want = r.emit(|c: &mut super::super::Compiler| c.emit_movss_xmm_xmm(xmm, src));
            let got = sel(&Req::new(
                Op::Movss,
                Ty::F32,
                Operand::Xmm(xmm),
                Operand::Xmm(src),
            ));
            assert_eq!(got, want, "MOVSS xmm{xmm}, xmm{src}");
        }
        let want = r.emit(|c: &mut super::super::Compiler| c.emit_pxor_xmm_self(xmm));
        let got = sel(&Req::new(
            Op::Pxor,
            Ty::I64,
            Operand::Xmm(xmm),
            Operand::None,
        ));
        assert_eq!(got, want, "PXOR xmm{xmm}");

        for depth in [0i32, 8, 127, 128, 1024] {
            let want = r.emit(|c: &mut super::super::Compiler| {
                c.emit_movq_mem_rbp_from_xmm(depth, xmm)
            });
            let mem = Mem::base_disp(RBP, -i64::from(depth));
            let got = sel(&Req::new(
                Op::Movq,
                Ty::I64,
                Operand::Mem(mem),
                Operand::Xmm(xmm),
            ));
            assert_eq!(got, want, "MOVQ [rbp - {depth}], xmm{xmm}");

            let want = r.emit(|c: &mut super::super::Compiler| {
                c.emit_movq_xmm_from_mem_rbp(xmm, depth)
            });
            let got = sel(&Req::new(
                Op::Movq,
                Ty::I64,
                Operand::Xmm(xmm),
                Operand::Mem(mem),
            ));
            assert_eq!(got, want, "MOVQ xmm{xmm}, [rbp - {depth}]");
        }
    }
    let want = r.emit(|c: &mut super::super::Compiler| c.emit_sqrtsd_xmm0());
    let got = sel(&Req::new(
        Op::Sqrtsd,
        Ty::F64,
        Operand::Xmm(0),
        Operand::Xmm(0),
    ));
    assert_eq!(got, want, "SQRTSD xmm0, xmm0");
    assert_eq!(want, vec![0xF2, 0x0F, 0x51, 0xC0]);
}

/// The rows whose ground truth is an inline byte literal rather than a
/// named emitter. The literal and its `x64.rs` location are quoted in the
/// row's `emitter` field.
#[test]
fn inline_byte_literal_rows_reproduce_those_literals() {
    // SHL/SAR/SHR EAX, CL — the `ishl`/`ishr`/`iushr` arithmetic steps.
    assert_eq!(
        sel(&Req::new(
            Op::Shl,
            Ty::I32,
            Operand::Gpr(RAX),
            Operand::None
        )),
        vec![0xD3, 0xE0]
    );
    assert_eq!(
        sel(&Req::new(
            Op::Sar,
            Ty::I32,
            Operand::Gpr(RAX),
            Operand::None
        )),
        vec![0xD3, 0xF8]
    );
    assert_eq!(
        sel(&Req::new(
            Op::Shr,
            Ty::I32,
            Operand::Gpr(RAX),
            Operand::None
        )),
        vec![0xD3, 0xE8]
    );
    // IMUL EAX, ECX and the MOVSXD RAX, EAX that follows it.
    assert_eq!(
        sel(&gpr(Op::Imul, Ty::I32, RAX, RCX)),
        vec![0x0F, 0xAF, 0xC1]
    );
    assert_eq!(
        sel(&gpr(Op::Movsxd, Ty::I32, RAX, RAX)),
        vec![0x48, 0x63, 0xC0]
    );
    // AND EAX, ECX through the parametric ALU row.
    assert_eq!(
        sel(&gpr(Op::Alu, Ty::I32, RAX, RCX).with_extra(Operand::OpByte(0x21))),
        vec![0x21, 0xC8]
    );
    // CQO ; IDIV RCX  and  CDQ ; IDIV ECX.
    assert_eq!(
        sel(&Req::new(
            Op::SignExtendAcc,
            Ty::I64,
            Operand::None,
            Operand::None
        )),
        vec![0x48, 0x99]
    );
    assert_eq!(
        sel(&Req::new(
            Op::Idiv,
            Ty::I64,
            Operand::Gpr(RCX),
            Operand::None
        )),
        vec![0x48, 0xF7, 0xF9]
    );
    assert_eq!(
        sel(&Req::new(
            Op::SignExtendAcc,
            Ty::I32,
            Operand::None,
            Operand::None
        )),
        vec![0x99]
    );
    assert_eq!(
        sel(&Req::new(
            Op::Idiv,
            Ty::I32,
            Operand::Gpr(RCX),
            Operand::None
        )),
        vec![0xF7, 0xF9]
    );
}

// ── displacement forms ────────────────────────────────────────────────

/// Every memory row must resolve its displacement through `disp.rs` and
/// must round-trip: what the decoder reads back is what the caller asked
/// for, and the `mod` field agrees with the emitted width.
#[test]
fn displacement_forms_round_trip_through_disp() {
    let values: [i64; 13] = [
        0,
        1,
        -1,
        8,
        127,
        -128,
        128,
        -129,
        1024,
        -1024,
        65_536,
        i64::from(i32::MAX),
        i64::from(i32::MIN),
    ];
    for p in PATTERNS {
        if !p.is_memory() {
            continue;
        }
        for &base in REGS.iter() {
            if p.constraints.contains(&Constraint::NoSibBase) && base_requires_sib(base) {
                continue;
            }
            for &v in values.iter() {
                let mem = Mem {
                    base,
                    index: if p.constraints.contains(&Constraint::IndexNotRsp) {
                        Some(Index { reg: RCX, scale: 1 })
                    } else {
                        None
                    },
                    disp: v,
                    force_disp32: matches!(p.disp, DispPolicy::Force32),
                };
                let a = Args {
                    dst: RAX,
                    src: RCX,
                    mem,
                    imm: 0,
                    op_byte: 0,
                };
                let e = match p.encode(&a) {
                    Ok(e) => e,
                    Err(err) => panic!("`{}` base={base} disp={v}: {err}", p.name),
                };
                let d = decode(&e.bytes, p.enc.modrm)
                    .unwrap_or_else(|| panic!("`{}` produced undecodable bytes", p.name));
                assert_eq!(
                    d.disp, v,
                    "`{}` base={base}: the CPU would read {} where {v} was asked for",
                    p.name, d.disp
                );
                assert_eq!(d.base(), Some(base), "`{}` lost the base register", p.name);
                // `mod` and the emitted width must never disagree.
                let expect_len = match d.mod_bits() {
                    Some(0b00) => 0,
                    Some(0b01) => 1,
                    Some(0b10) => 4,
                    other => panic!("`{}`: illegal mod field {other:?}", p.name),
                };
                assert_eq!(d.disp_len, expect_len, "`{}` mod/width disagree", p.name);
                // The forced-width rows must stay four bytes wide even
                // for a displacement of zero: their length is load-bearing.
                if matches!(p.disp, DispPolicy::Force32) {
                    assert_eq!(d.disp_len, 4, "`{}` must stay disp32", p.name);
                }
                // RBP/R13 have no `mod=00` form.
                if super::super::base_requires_displacement(base) {
                    assert_ne!(
                        d.mod_bits(),
                        Some(0b00),
                        "`{}` with base r{base} would decode as RIP-relative",
                        p.name
                    );
                }
                // RSP/R12 cannot be named without a SIB byte.
                if base_requires_sib(base) {
                    assert!(d.sib.is_some(), "`{}` base r{base} needs a SIB", p.name);
                }
            }
        }
    }
}

/// A displacement past disp32 has no encoding; every memory row must
/// refuse it rather than truncate.
#[test]
fn unencodable_displacements_are_refused_by_every_memory_row() {
    for p in PATTERNS {
        if !p.is_memory() {
            continue;
        }
        let mem = Mem {
            base: RAX,
            index: if p.constraints.contains(&Constraint::IndexNotRsp) {
                Some(Index { reg: RCX, scale: 1 })
            } else {
                None
            },
            disp: 1i64 << 40,
            force_disp32: matches!(p.disp, DispPolicy::Force32),
        };
        let a = Args {
            dst: RAX,
            src: RCX,
            mem,
            imm: 0,
            op_byte: 0,
        };
        assert_eq!(
            p.encode(&a),
            Err(SelError::Disp(DispOutOfRange { value: 1i64 << 40 })),
            "`{}` must refuse a displacement with no encoding",
            p.name
        );
    }
}

// ── constraints ───────────────────────────────────────────────────────

/// Constraint violations must come back as errors, not as bytes.
#[test]
fn constraint_violations_are_rejected_rather_than_encoded() {
    let base_args = Args {
        dst: RAX,
        src: RCX,
        mem: Mem::base_disp(RAX, 0),
        imm: 0,
        op_byte: 0,
    };

    // An RSP base in an encoding that emits no SIB byte would silently
    // become "a SIB byte follows" and misparse everything after it.
    let mut a = base_args;
    a.mem = Mem::base_disp(RSP, 0);
    a.imm = 0x80;
    assert_eq!(
        encode_named("test_m8_imm8", &a),
        Err(SelError::Constraint {
            pattern: "test_m8_imm8",
            constraint: Constraint::NoSibBase,
        })
    );

    // RSP can never be a SIB index.
    let mut a = base_args;
    a.mem = Mem::base_index(RAX, RSP, 1, 0);
    assert_eq!(
        encode_named("mov_m8_index_imm8", &a),
        Err(SelError::Constraint {
            pattern: "mov_m8_index_imm8",
            constraint: Constraint::IndexNotRsp,
        })
    );

    // A scale x86 cannot express.
    let mut a = base_args;
    a.mem = Mem::base_index(RAX, RCX, 3, 0);
    assert_eq!(
        encode_named("mov_m8_index_imm8", &a),
        Err(SelError::BadScale { scale: 3 })
    );

    // `TEST r,r` is a same-register idiom.
    let a = base_args;
    assert_eq!(
        encode_named("test_r64_r64", &a),
        Err(SelError::Constraint {
            pattern: "test_r64_r64",
            constraint: Constraint::SameRegister,
        })
    );

    // The condition byte families are disjoint: a Jcc byte is not a CMOV
    // byte, and swapping them would emit a different instruction.
    let mut a = base_args;
    a.op_byte = 0x84;
    assert_eq!(
        encode_named("cmov_r64_r64", &a),
        Err(SelError::Constraint {
            pattern: "cmov_r64_r64",
            constraint: Constraint::CcIsCmov,
        })
    );
    a.op_byte = 0x44;
    assert_eq!(
        encode_named("jcc_rel32", &a),
        Err(SelError::Constraint {
            pattern: "jcc_rel32",
            constraint: Constraint::CcIsJcc,
        })
    );

    // Register numbers outside the file.
    let mut a = base_args;
    a.dst = 16;
    assert_eq!(
        encode_named("mov_r64_r64", &a),
        Err(SelError::BadRegister {
            pattern: "mov_r64_r64",
            reg: 16,
        })
    );

    // An immediate that does not fit the declared field.
    let mut a = base_args;
    a.imm = 200;
    assert_eq!(
        encode_named("add_r64_imm8", &a),
        Err(SelError::Constraint {
            pattern: "add_r64_imm8",
            constraint: Constraint::ImmFitsI8,
        })
    );

    // A shift count past 63 is not a legal 64-bit shift.
    let mut a = base_args;
    a.imm = 64;
    assert_eq!(
        encode_named("shr_r64_imm8", &a),
        Err(SelError::Constraint {
            pattern: "shr_r64_imm8",
            constraint: Constraint::ShiftCountFits64,
        })
    );

    // And a request no row covers is an error, not a guess.
    assert!(matches!(
        select(&Req::new(
            Op::Sqrtsd,
            Ty::I8,
            Operand::Xmm(0),
            Operand::Xmm(0)
        )),
        Err(SelError::NoPattern { .. })
    ));
    assert_eq!(
        encode_named("no_such_pattern", &base_args),
        Err(SelError::UnknownPattern)
    );
}

// ── immediate-size selection ──────────────────────────────────────────

/// The selector must pick the smallest legal immediate form, and the
/// boundaries must be the *signed* ones.
#[test]
fn immediate_size_selection_picks_the_smallest_legal_form() {
    // MOV r64, imm: zero -> XOR, 1..=u32::MAX -> zero-extending B8+rd
    // imm32, negative i32 -> C7, wider -> B8+rd imm64.
    assert_eq!(
        select(&gpr_imm(Op::Mov, Ty::I64, RAX, 0))
            .expect("zero")
            .pattern
            .name,
        "mov_r64_imm0_xor"
    );
    for v in [
        1i64,
        127,
        128,
        i64::from(i32::MAX),
        i64::from(i32::MAX) + 1,
        i64::from(u32::MAX),
    ] {
        assert_eq!(
            select(&gpr_imm(Op::Mov, Ty::I64, RAX, v))
                .expect("u32 range")
                .pattern
                .name,
            "mov_r32_imm32_zx",
            "value {v}"
        );
    }
    for v in [-1i64, -128, i64::from(i32::MIN)] {
        assert_eq!(
            select(&gpr_imm(Op::Mov, Ty::I64, RAX, v))
                .expect("negative i32 range")
                .pattern
                .name,
            "mov_r64_imm32",
            "value {v}"
        );
    }
    for v in [i64::from(u32::MAX) + 1, i64::from(i32::MIN) - 1, i64::MAX] {
        assert_eq!(
            select(&gpr_imm(Op::Mov, Ty::I64, RAX, v))
                .expect("wide")
                .pattern
                .name,
            "mov_r64_imm64",
            "value {v}"
        );
    }

    // ADD/SUB r64, imm: imm8 up to the SIGNED boundary, then imm32.
    for (op, small, big) in [
        (Op::Add, "add_r64_imm8", "add_r64_imm32"),
        (Op::Sub, "sub_r64_imm8", "sub_r64_imm32"),
    ] {
        for v in [0i64, 1, -1, 127, -128] {
            assert_eq!(
                select(&gpr_imm(op, Ty::I64, RSP, v))
                    .expect("imm8")
                    .pattern
                    .name,
                small,
                "{op:?} {v} must use the byte form"
            );
        }
        for v in [128i64, -129, 4096, i64::from(i32::MAX)] {
            assert_eq!(
                select(&gpr_imm(op, Ty::I64, RSP, v))
                    .expect("imm32")
                    .pattern
                    .name,
                big,
                "{op:?} {v} does not fit a SIGNED byte"
            );
        }
        assert!(
            select(&gpr_imm(op, Ty::I64, RSP, i64::from(i32::MAX) + 1)).is_err(),
            "{op:?}: no row can express an immediate past i32"
        );
    }

    // 128 is the value the whole `disp.rs` exercise exists for: it is a
    // legal u8 and NOT a legal signed imm8.
    let short = select(&gpr_imm(Op::Add, Ty::I64, RAX, 127)).expect("127");
    let long = select(&gpr_imm(Op::Add, Ty::I64, RAX, 128)).expect("128");
    assert_eq!(short.encoded.bytes.len() + 3, long.encoded.bytes.len());

    // The zeroing idiom is NOT flag-neutral. `emit_mov_imm64` substitutes
    // it unconditionally and so does the selector, which is faithful — but
    // the table records the difference, so a future caller that needs the
    // flags preserved across the move has something to consult instead of
    // reading the emitter.
    assert!(pattern("mov_r64_imm0_xor").expect("row").flags.writes);
    assert!(!pattern("mov_r64_imm32").expect("row").flags.writes);
    assert!(!pattern("mov_r32_imm32_zx").expect("row").flags.writes);
    assert!(!pattern("mov_r64_imm64").expect("row").flags.writes);
}

/// The smallest-form and forced-width memory rows are different answers
/// to different questions; the matcher must not substitute one for the
/// other.
#[test]
fn forced_width_and_smallest_form_rows_do_not_shadow_each_other() {
    let smallest = select(&Req::new(
        Op::Mov,
        Ty::I64,
        Operand::Gpr(RAX),
        Operand::Mem(Mem::base_disp(RCX, 8)),
    ))
    .expect("smallest");
    assert_eq!(smallest.pattern.name, "mov_r64_m");
    assert_eq!(smallest.encoded.bytes.len(), 4);

    let forced = select(&Req::new(
        Op::Mov,
        Ty::I64,
        Operand::Gpr(RAX),
        Operand::Mem(Mem::base_disp32(RCX, 8)),
    ))
    .expect("forced");
    assert_eq!(forced.pattern.name, "mov_r64_m_disp32");
    assert_eq!(forced.encoded.bytes.len(), 7);
}

// ── round-trip disassembly ────────────────────────────────────────────

/// Encode, then take the bytes apart again with a decoder that knows only
/// x86's structural rules, and check that every operand comes back.
#[test]
fn round_trip_disassembly_recovers_every_operand() {
    for p in PATTERNS {
        for &r1 in REGS.iter() {
            for &r2 in REGS.iter() {
                if p.constraints.contains(&Constraint::NoSibBase) && base_requires_sib(r2) {
                    continue;
                }
                if p.constraints.contains(&Constraint::IndexNotRsp) && r2 == RSP {
                    continue;
                }
                let a = args_for(p, r1, r2, r2, RCX, 64);
                let mem = a.mem;
                let e = match p.encode(&a) {
                    Ok(e) => e,
                    Err(err) => panic!("`{}` r{r1}/r{r2}: {err}", p.name),
                };
                if e.bytes.is_empty() {
                    // The elided self-move: nothing to decode.
                    assert_eq!(p.peephole, Peephole::ElideWhenDstEqSrc);
                    assert_eq!(a.dst, a.src);
                    continue;
                }
                let d = decode(&e.bytes, p.enc.modrm)
                    .unwrap_or_else(|| panic!("`{}` produced undecodable bytes", p.name));
                let text = render(p, &d);
                assert_eq!(d.len, e.bytes.len(), "{text}: decoder length");
                assert!(
                    !text.is_empty(),
                    "every pattern must disassemble to something readable"
                );

                // The opcode came back intact.
                match p.enc.opcode {
                    Opcode::One(b) => assert_eq!(d.opcode, vec![b], "`{}`", p.name),
                    Opcode::Two(b) => assert_eq!(d.opcode, vec![0x0F, b], "`{}`", p.name),
                    Opcode::PlusReg(b) => {
                        assert_eq!(d.opcode, vec![b | (a.dst & 7)], "`{}`", p.name)
                    }
                    Opcode::OperandOne => {
                        assert_eq!(d.opcode, vec![a.op_byte], "`{}`", p.name)
                    }
                    Opcode::OperandTwo => {
                        assert_eq!(d.opcode, vec![0x0F, a.op_byte], "`{}`", p.name)
                    }
                }
                assert_eq!(d.wide(), p.enc.rex_w, "`{}` REX.W", p.name);

                // The register fields came back intact.
                if p.enc.modrm {
                    let want_reg = match p.enc.reg {
                        RegF::Dst => a.dst,
                        RegF::Src => a.src,
                        RegF::Ext(n) => n,
                    };
                    assert_eq!(d.reg(), Some(want_reg), "`{}` reg field", p.name);
                    match p.enc.rm {
                        RmF::RegDst => {
                            assert_eq!(d.mod_bits(), Some(0b11), "`{}`", p.name);
                            assert_eq!(d.rm_reg(), Some(a.dst), "`{}` r/m", p.name);
                        }
                        RmF::RegSrc => {
                            assert_eq!(d.mod_bits(), Some(0b11), "`{}`", p.name);
                            assert_eq!(d.rm_reg(), Some(a.src), "`{}` r/m", p.name);
                        }
                        RmF::Mem => {
                            assert_eq!(d.base(), Some(mem.base), "`{}` base", p.name);
                            assert_eq!(d.disp, mem.disp, "`{}` disp", p.name);
                            match mem.index {
                                Some(ix) => {
                                    assert_eq!(d.index(), Some(ix.reg), "`{}`", p.name);
                                    assert_eq!(d.scale(), Some(ix.scale), "`{}`", p.name);
                                }
                                None => assert_eq!(d.index(), None, "`{}`", p.name),
                            }
                        }
                        RmF::None => {}
                    }
                }

                // The immediate came back intact and at the offset the
                // pattern advertised.
                assert_eq!(
                    d.imm.len(),
                    p.imm.byte_len(),
                    "`{}` immediate width",
                    p.name
                );
                if p.imm.byte_len() > 0 {
                    assert_eq!(
                        e.imm_offset,
                        Some(e.bytes.len() - p.imm.byte_len()),
                        "`{}` immediate offset",
                        p.name
                    );
                }
            }
        }
    }
}

/// Spot-check the decoder itself against hand-verified encodings, so a
/// bug in the decoder cannot quietly validate a bug in the encoder.
#[test]
fn the_decoder_agrees_with_hand_verified_bytes() {
    // 4C 8B 7C 24 08 = MOV R15, [RSP + 8]
    let d = decode(&[0x4C, 0x8B, 0x7C, 0x24, 0x08], true).expect("decodable");
    assert_eq!(d.rex, Some(0x4C));
    assert!(d.wide());
    assert_eq!(d.opcode, vec![0x8B]);
    assert_eq!(d.reg(), Some(R15));
    assert_eq!(d.base(), Some(RSP));
    assert_eq!(d.index(), None);
    assert_eq!(d.disp, 8);
    assert_eq!(d.mod_bits(), Some(0b01));

    // 49 8B 45 00 = MOV RAX, [R13 + 0] — the base with no mod=00 form.
    let d = decode(&[0x49, 0x8B, 0x45, 0x00], true).expect("decodable");
    assert_eq!(d.reg(), Some(RAX));
    assert_eq!(d.base(), Some(R13));
    assert_eq!(d.disp, 0);
    assert_eq!(d.disp_len, 1);

    // 42 C6 04 08 FF = MOV BYTE [RAX + R9], 0xFF (REX.X only).
    let d = decode(&[0x42, 0xC6, 0x04, 0x08, 0xFF], true).expect("decodable");
    assert_eq!(d.base(), Some(RAX));
    assert_eq!(d.index(), Some(R9));
    assert_eq!(d.scale(), Some(1));
    assert_eq!(d.imm, vec![0xFF]);

    // 0F 8F 00 00 00 00 = JG rel32, no ModRM.
    let d = decode(&[0x0F, 0x8F, 0, 0, 0, 0], false).expect("decodable");
    assert_eq!(d.opcode, vec![0x0F, 0x8F]);
    assert_eq!(d.modrm, None);
    assert_eq!(d.imm, vec![0, 0, 0, 0]);

    // Truncated input is refused rather than mis-parsed.
    assert_eq!(decode(&[0x48], true), None);
    assert_eq!(decode(&[0x48, 0x8B], true), None);
    assert_eq!(decode(&[0x48, 0x8B, 0x80, 0x00], true), None);
    assert_eq!(decode(&[], false), None);

    // A disp8 byte is read back SIGNED — the property `disp.rs` exists to
    // preserve, restated at the decoder so the round trip is meaningful.
    let d = decode(&[0x48, 0x8B, 0x41, 0x80], true).expect("decodable");
    assert_eq!(d.disp, -128, "0x80 is -128, not 128");
}

/// A pattern whose bytes cannot be decoded structurally would make every
/// round-trip assertion vacuous.
#[test]
fn every_pattern_produces_structurally_decodable_bytes() {
    let mut decoded_rows = 0usize;
    for p in PATTERNS {
        let a = args_for(p, R8, R11, RBX, RDI, -8);
        let e = match p.encode(&a) {
            Ok(e) => e,
            Err(err) => panic!("`{}`: {err}", p.name),
        };
        assert!(
            decode(&e.bytes, p.enc.modrm).is_some(),
            "`{}` emitted bytes no decoder can parse: {:02X?}",
            p.name,
            e.bytes
        );
        decoded_rows += 1;
    }
    assert_eq!(decoded_rows, PATTERNS.len());
}

// ══════════════════════════════════════════════════════════════════════
// IR-level instruction selection
// ══════════════════════════════════════════════════════════════════════

use crate::ir::{MemKind, SafepointSnapshot, NO_NODE};

/// A bare graph with no entry, as `ir_schedule`'s `bare_graph()` builds one.
fn g() -> Graph {
    let mut g = Graph::empty(0);
    g.entry = NO_NODE;
    g
}

fn konst(graph: &mut Graph, v: i64) -> NodeId {
    graph.add(IrOp::Const(v), IrType::Long, vec![], None)
}

fn konst_i32(graph: &mut Graph, v: i64) -> NodeId {
    graph.add(IrOp::Const(v), IrType::Int, vec![], None)
}

fn param(graph: &mut Graph, i: u16) -> NodeId {
    graph.add(IrOp::Param(i), IrType::Long, vec![], None)
}

fn bin(graph: &mut Graph, op: IrOp, a: NodeId, b: NodeId) -> NodeId {
    graph.add(op, IrType::Long, vec![a, b], None)
}

/// Give `id` a second consumer so it is no longer single-use. Returns the
/// consumer, which the caller can leave out of the block under test.
fn second_use(graph: &mut Graph, id: NodeId) -> NodeId {
    graph.add(IrOp::Neg, IrType::Long, vec![id], None)
}

fn ctx_of<'a>(graph: &'a Graph, block: &[NodeId]) -> SelCtx<'a> {
    SelCtx::new(graph, block)
}

// ── the new ALU rows are the byte literals they claim ─────────────────

/// Every row added for the IR selector names an `ir_lower.rs` byte
/// literal in its `emitter` field. This is that claim, checked: if a row
/// and the literal it cites ever disagree, the table is lying about what
/// the compiler emits and the selector's cost model is priced off a
/// fiction.
#[test]
fn the_new_alu_rows_reproduce_the_ir_lower_byte_literals() {
    // `ir_lower::lower_data_node`, Op::Add / Sub / Mul / And / Or / Xor.
    assert_eq!(
        sel(&gpr(Op::Add, Ty::I64, RAX, RCX)),
        vec![0x48, 0x01, 0xC8]
    );
    assert_eq!(sel(&gpr(Op::Add, Ty::I32, RAX, RCX)), vec![0x01, 0xC8]);
    assert_eq!(
        sel(&gpr(Op::Sub, Ty::I64, RAX, RCX)),
        vec![0x48, 0x29, 0xC8]
    );
    assert_eq!(sel(&gpr(Op::Sub, Ty::I32, RAX, RCX)), vec![0x29, 0xC8]);
    assert_eq!(
        sel(&gpr(Op::And, Ty::I64, RAX, RCX)),
        vec![0x48, 0x21, 0xC8]
    );
    assert_eq!(sel(&gpr(Op::Or, Ty::I64, RAX, RCX)), vec![0x48, 0x09, 0xC8]);
    assert_eq!(
        sel(&gpr(Op::Xor, Ty::I64, RAX, RCX)),
        vec![0x48, 0x31, 0xC8]
    );
    // The `Int` bitwise arms are 32-bit (no REX.W) — see `alu_op_of`.
    assert_eq!(sel(&gpr(Op::And, Ty::I32, RAX, RCX)), vec![0x21, 0xC8]);
    assert_eq!(sel(&gpr(Op::Or, Ty::I32, RAX, RCX)), vec![0x09, 0xC8]);
    assert_eq!(sel(&gpr(Op::Xor, Ty::I32, RAX, RCX)), vec![0x31, 0xC8]);
    assert_eq!(
        sel(&gpr(Op::Imul, Ty::I64, RAX, RCX)),
        vec![0x48, 0x0F, 0xAF, 0xC1]
    );
    assert_eq!(
        sel(&gpr(Op::Imul, Ty::I32, RAX, RCX)),
        vec![0x0F, 0xAF, 0xC1]
    );
}

/// An `Int` `&` / `|` / `^` selects the 32-bit row, never the REX.W one.
///
/// `alu_op_of` used to widen the three bitwise ops to `Ty::I64` for an
/// `Int` node. `ir_lower` emits the 32-bit form for exactly those nodes so
/// the int home word stays the zero-extension of its low half; the REX.W
/// form over a sign-extended and a zero-extended operand produces a word
/// that is neither (an out-of-bounds index that passes a low-half bounds
/// check). Under `ir-isel-emit` that tile was the emitted code.
///
/// The exact edit that trips it: put `Ty::I64` back in any of the three
/// `alu_op_of` arms.
#[test]
fn an_int_bitwise_node_selects_the_32bit_form() {
    for (op, want) in [
        (IrOp::And, [0x21u8, 0xC8]),
        (IrOp::Or, [0x09, 0xC8]),
        (IrOp::Xor, [0x31, 0xC8]),
    ] {
        let mut graph = g();
        let p = graph.add(IrOp::Param(0), IrType::Int, vec![], None);
        let q = graph.add(IrOp::Param(1), IrType::Int, vec![], None);
        let n = graph.add(op.clone(), IrType::Int, vec![p, q], None);
        let block = vec![n];
        let s = select_block(&graph, &block, None, &SelectOptions::default());
        assert!(s.covers(&block), "coverage: {s:?}");
        let t = s.tiles.iter().find(|t| t.root == n).expect("root");
        let alu = t.insts.iter().find_map(|i| match *i {
            MInst::AluRR { ty, .. } => Some((i, ty)),
            _ => None,
        });
        let (inst, ty) = alu.unwrap_or_else(|| panic!("{op:?}: no AluRR in {t:?}"));
        assert_eq!(ty, Ty::I32, "{op:?} on an Int node must stay 32-bit");
        // Placeholder registers RAX/RCX: exactly `ir_lower`'s literal.
        let bytes = inst
            .probe()
            .unwrap_or_else(|e| panic!("{op:?}: {e:?}"))
            .bytes;
        assert_eq!(bytes, want.to_vec(), "{op:?}: no REX.W on an Int");
    }
    // And a Long node still takes the wide form.
    let mut graph = g();
    let p = param(&mut graph, 0);
    let q = param(&mut graph, 1);
    let n = bin(&mut graph, IrOp::And, p, q);
    let s = select_block(&graph, &[n], None, &SelectOptions::default());
    let t = s.tiles.iter().find(|t| t.root == n).expect("root");
    assert!(t
        .insts
        .iter()
        .any(|i| matches!(i, MInst::AluRR { ty: Ty::I64, .. })));
}

/// RSP is never a SIB index, on ANY memory row — including the two `LEA`
/// rows that never declared `Constraint::IndexNotRsp` and would otherwise
/// have encoded `[base + rsp*1]` as `[base]`. R12 *is* a legal index
/// (REX.X distinguishes it), so it must still encode.
#[test]
fn an_rsp_index_is_refused_by_every_indexed_row() {
    for name in ["lea_r64_m", "lea_r32_m", "mov_m8_index_imm8"] {
        let p = pattern(name).unwrap_or_else(|| panic!("{name} missing"));
        let mut a = Args {
            dst: RAX,
            src: RCX,
            mem: Mem::base_index(RAX, RSP, 1, 0),
            imm: 1,
            op_byte: 0,
        };
        assert_eq!(
            p.encode(&a),
            Err(SelError::Constraint {
                pattern: p.name,
                constraint: Constraint::IndexNotRsp,
            }),
            "{name} must refuse an RSP index"
        );
        a.mem = Mem::base_index(RAX, R12, 1, 0);
        let e = p
            .encode(&a)
            .unwrap_or_else(|e| panic!("{name} must accept an R12 index: {e}"));
        let d = decode(&e.bytes, p.enc.modrm).expect("decodable");
        assert_eq!(d.index(), Some(R12), "{name}: R12 index round-trips");
    }
}

#[test]
fn variable_shifts_select_a_fixed_cl_count() {
    for (ir_op, machine_op) in [
        (IrOp::Shl, Op::Shl),
        (IrOp::Shr, Op::Sar),
        (IrOp::UShr, Op::Shr),
    ] {
        for (ir_ty, ty) in [(IrType::Int, Ty::I32), (IrType::Long, Ty::I64)] {
            let mut graph = g();
            let value = graph.add(IrOp::Param(0), ir_ty, vec![], None);
            let count = graph.add(IrOp::Param(1), IrType::Int, vec![], None);
            let root = graph.add(ir_op.clone(), ir_ty, vec![value, count], None);
            let selection = select_block(&graph, &[root], None, &SelectOptions::default());
            let instruction = selection.tiles[0].insts.last().expect("shift");
            assert!(matches!(
                *instruction,
                MInst::AluRR { op, ty: chosen_ty, lhs, rhs, .. }
                    if op == machine_op && chosen_ty == ty && lhs == value && rhs == count
            ), "variable shift is missing its fixed operand: {instruction:?}");
            let row = instruction.pattern_name().and_then(pattern).expect("shift row");
            assert_eq!(row.fixed, FIXED_SHIFT_CL);
            assert!(instruction.probe().is_ok());
        }
    }
}

#[test]
fn immediate_shifts_select_the_java_masked_count() {
    for (ir_op, machine_op) in [
        (IrOp::Shl, Op::Shl),
        (IrOp::Shr, Op::Sar),
        (IrOp::UShr, Op::Shr),
    ] {
        for (ir_ty, ty, mask) in [(IrType::Int, Ty::I32, 31), (IrType::Long, Ty::I64, 63)] {
            for count in [-1, 32, 64, 65, i64::from(i32::MAX)] {
                let mut graph = g();
                let value = graph.add(IrOp::Param(0), ir_ty, vec![], None);
                let shift = graph.add(IrOp::Const(count), IrType::Int, vec![], None);
                let root = graph.add(ir_op.clone(), ir_ty, vec![value, shift], None);
                graph.add(IrOp::Neg, IrType::Int, vec![shift], None);
                let block = [shift, root];
                let selected = select_block(&graph, &block, None, &SelectOptions::default());
                assert!(selected.covers(&block));
                let tile = selected.tiles.iter().find(|t| t.root == root).expect("root");
                assert_eq!(tile.covered, [root]);
                assert!(matches!(
                    tile.insts.last(),
                    Some(MInst::AluRI { op, ty: chosen_ty, imm, form: ImmForm::ImmU8, .. })
                        if *op == machine_op && *chosen_ty == ty && *imm == count & mask
                ), "wrong shift form: {tile:?}");
                assert!(tile.insts.last().expect("shift").probe().is_ok());
            }
        }
    }
}

/// The zeroing idiom and a real `XOR` share an opcode; they must not share
/// a row, or a request for `xor a, b` could be answered with `a ^ a`.
/// Page `r11-x64gen-isel-select-ignores-flag-effects`: the default request
/// for `MOV r, 0` is the flag-writing `XOR`; a flags-live request gets a
/// flag-neutral row, and an op whose every row writes flags gets none.
#[test]
fn a_flags_live_request_never_gets_a_flag_writing_row() {
    let plain = select(&gpr_imm(Op::Mov, Ty::I64, RAX, 0)).expect("zero");
    assert!(plain.pattern.flags.writes, "the default is the XOR idiom");
    let live = select(&gpr_imm(Op::Mov, Ty::I64, RAX, 0).preserving_flags())
        .expect("a flag-neutral MOV exists");
    assert!(!live.pattern.flags.writes, "{}", live.pattern.name);
    assert_ne!(live.pattern.name, "mov_r64_imm0_xor");
    assert!(select(&gpr(Op::Add, Ty::I64, RAX, RCX).preserving_flags()).is_err());
}

#[test]
fn the_xor_zeroing_idiom_and_a_real_xor_are_different_rows() {
    let zeroing = select(&gpr_imm(Op::Mov, Ty::I64, RDX, 0)).expect("zeroing");
    assert_eq!(zeroing.pattern.name, "mov_r64_imm0_xor");
    let real = select(&gpr(Op::Xor, Ty::I64, RDX, RSI)).expect("xor");
    assert_eq!(real.pattern.name, "xor_r64_r64");
    assert_ne!(zeroing.encoded.bytes, real.encoded.bytes);
}

// ── immediate widths ──────────────────────────────────────────────────

#[test]
fn method_selection_does_not_keep_previous_block_claims_or_positions() {
    let mut graph = g();
    let base = param(&mut graph, 0);
    let constant = konst(&mut graph, 24);
    let add = bin(&mut graph, IrOp::Add, base, constant);
    let opts = SelectOptions::default();
    let mut selector = MethodSelector::new(&graph);
    let full = selector.select_block(&[constant, add], None, &opts);
    assert_eq!(full.tiles.len(), 1);
    assert!(full.tiles[0].covered.contains(&constant));
    for block in [&[constant][..], &[add], &[constant, add]] {
        let reused = selector.select_block(block, None, &opts);
        assert_eq!(reused, select_block(&graph, block, None, &opts));
    }
}

/// The whole point of `imm_form_for`: 128 is NOT an `imm8`. A raw
/// `128 as i8` is `-128`, so `AND r, 128` written that way clears every
/// bit but the sign bit instead of setting one.
#[test]
fn immediate_widths_use_checked_conversions_not_casts() {
    assert_eq!(imm_form_for(0), Some(ImmForm::Imm8));
    assert_eq!(imm_form_for(127), Some(ImmForm::Imm8));
    assert_eq!(imm_form_for(-128), Some(ImmForm::Imm8));
    assert_eq!(imm_form_for(128), Some(ImmForm::Imm32));
    assert_eq!(imm_form_for(-129), Some(ImmForm::Imm32));
    assert_eq!(imm_form_for(i64::from(i32::MAX)), Some(ImmForm::Imm32));
    assert_eq!(imm_form_for(i64::from(i32::MIN)), Some(ImmForm::Imm32));
    assert_eq!(imm_form_for(i64::from(i32::MAX) + 1), None);
    assert_eq!(imm_form_for(i64::from(i32::MIN) - 1), None);
    assert_eq!(imm_form_for(i64::MAX), None);
    // Restate the bug the checked form prevents.
    assert_eq!(128u8 as i8, -128);
}

/// A constant too wide for any immediate field must fall back to the
/// register form, and say so — never be truncated into the field.
#[test]
fn a_constant_past_imm32_falls_back_to_the_register_form() {
    let mut graph = g();
    let p = param(&mut graph, 0);
    let big = konst(&mut graph, i64::from(i32::MAX) + 1);
    let add = bin(&mut graph, IrOp::Add, p, big);
    let block = vec![big, add];
    let s = select_block(&graph, &block, None, &SelectOptions::default());
    assert!(s.covers(&block), "coverage: {s:?}");
    let root = s
        .tiles
        .iter()
        .find(|t| t.root == add)
        .expect("the add is a tile root");
    assert_eq!(
        root.rule,
        Rule::AluReg,
        "a wide constant must not become an immediate"
    );
    assert!(
        s.notes.iter().any(|n| matches!(
            n,
            Note::WideImmediate { value, .. } if *value == i64::from(i32::MAX) + 1
        )),
        "the refusal must be reported: {:?}",
        s.notes
    );
}

#[test]
fn a_constant_that_fits_imm8_becomes_an_immediate() {
    let mut graph = g();
    let p = param(&mut graph, 0);
    let k = konst(&mut graph, 24);
    let add = bin(&mut graph, IrOp::Add, p, k);
    let block = vec![k, add];
    let s = select_block(&graph, &block, None, &SelectOptions::default());
    assert!(s.covers(&block));
    let root = s.tiles.iter().find(|t| t.root == add).expect("root");
    assert_eq!(root.rule, Rule::AluImm);
    assert!(
        root.covered.contains(&k),
        "the constant must be absorbed, not left to emit a dead MOV"
    );
    assert!(root.insts.iter().any(|i| matches!(
        i,
        MInst::AluRI {
            op: Op::Add,
            form: ImmForm::Imm8,
            ..
        }
    )));
}

// ── address-mode folding ──────────────────────────────────────────────

#[test]
fn address_folds_base_index_scale_and_displacement() {
    // p0 + (i << 2) + 16
    let mut graph = g();
    let p0 = param(&mut graph, 0);
    let i = param(&mut graph, 1);
    let two = konst(&mut graph, 2);
    let shl = bin(&mut graph, IrOp::Shl, i, two);
    let sixteen = konst(&mut graph, 16);
    let a1 = bin(&mut graph, IrOp::Add, p0, shl);
    let a2 = bin(&mut graph, IrOp::Add, a1, sixteen);
    let block = vec![two, shl, sixteen, a1, a2];
    let ctx = ctx_of(&graph, &block);
    let m = match_address(&ctx, a2).expect("an address");
    assert_eq!(m.addr.base, Some(p0));
    assert_eq!(m.addr.index, Some(i));
    assert_eq!(m.addr.scale, 4);
    assert_eq!(m.addr.disp, 16);
    assert_eq!(m.addr.terms(), 3);
    for n in [a2, a1, shl, two, sixteen] {
        assert!(m.absorbed.contains(&n), "node {n} must be absorbed");
    }
    assert!(m.addr.check().is_ok());
}

#[test]
fn a_shared_constant_is_a_displacement_without_absorbing_its_definition() {
    let mut graph = g();
    let base = param(&mut graph, 0);
    let constant = konst(&mut graph, 24);
    let add = bin(&mut graph, IrOp::Add, base, constant);
    let _keep_base = second_use(&mut graph, base);
    let _keep_constant = second_use(&mut graph, constant);
    let block = [constant, add];
    let selection = select_block(&graph, &block, None, &SelectOptions::default());
    assert!(selection.covers(&block));
    let tile = selection.tiles.iter().find(|tile| tile.root == add).expect("add");
    assert_eq!(tile.covered, [add]);
    assert!(matches!(
        tile.insts.as_slice(),
        [MInst::Lea { addr, .. }]
            if addr.base == Some(base) && addr.index.is_none() && addr.disp == 24
    ), "expected a displacement without a constant register: {tile:?}");
    assert!(selection.tiles.iter().any(|tile| tile.root == constant));
}

/// An interior node with a second consumer keeps its register and becomes
/// a plain term. Absorbing it would compute it twice.
#[test]
fn a_shared_interior_node_is_not_absorbed() {
    let mut graph = g();
    let p0 = param(&mut graph, 0);
    let i = param(&mut graph, 1);
    let two = konst(&mut graph, 2);
    let shl = bin(&mut graph, IrOp::Shl, i, two);
    let add = bin(&mut graph, IrOp::Add, p0, shl);
    // A second consumer of the shift, outside the block under test.
    let _other = second_use(&mut graph, shl);
    let block = vec![two, shl, add];
    let ctx = ctx_of(&graph, &block);
    let m = match_address(&ctx, add).expect("an address");
    assert_eq!(m.addr.base, Some(p0));
    assert_eq!(
        m.addr.index,
        Some(shl),
        "the shared shift stays a value and becomes the index itself"
    );
    assert_eq!(m.addr.scale, 1, "its scale is gone with it");
    assert!(!m.absorbed.contains(&shl));
}

/// A node the scheduler put in another block must not be pulled in: it
/// would be recomputed here *and* still computed there.
#[test]
fn an_out_of_block_interior_node_is_not_absorbed() {
    let mut graph = g();
    let p0 = param(&mut graph, 0);
    let i = param(&mut graph, 1);
    let two = konst(&mut graph, 2);
    let shl = bin(&mut graph, IrOp::Shl, i, two);
    let add = bin(&mut graph, IrOp::Add, p0, shl);
    // `shl` is single-use but lives elsewhere.
    let block = vec![add];
    let ctx = ctx_of(&graph, &block);
    let m = match_address(&ctx, add).expect("an address");
    assert_eq!(m.addr.index, Some(shl));
    assert_eq!(m.addr.scale, 1);
    assert!(!m.absorbed.contains(&shl));
}

/// The checked-displacement gate. `Disp::encode32` refuses, so the address
/// is refused — it is never truncated into a 32-bit field.
#[test]
fn a_displacement_past_disp32_is_refused_not_truncated() {
    let mut graph = g();
    let p0 = param(&mut graph, 0);
    let far = konst(&mut graph, 1i64 << 40);
    let add = bin(&mut graph, IrOp::Add, p0, far);
    let block = vec![far, add];
    let ctx = ctx_of(&graph, &block);
    assert_eq!(
        match_address(&ctx, add),
        Err(AddrRefusal::Unencodable(SelError::Disp(DispOutOfRange {
            value: 1i64 << 40
        })))
    );
    // And the selector must not choose an LEA it cannot encode.
    let s = select_block(&graph, &block, None, &SelectOptions::default());
    assert!(s.covers(&block));
    assert!(
        !s.matched().any(|t| t.rule == Rule::Lea),
        "no LEA may survive an unencodable displacement"
    );
}

#[test]
fn a_folded_displacement_cannot_wrap() {
    let mut graph = g();
    let p0 = param(&mut graph, 0);
    let a = konst(&mut graph, i64::MAX);
    let b = konst(&mut graph, 1);
    let s1 = bin(&mut graph, IrOp::Add, p0, a);
    let s2 = bin(&mut graph, IrOp::Add, s1, b);
    let block = vec![a, b, s1, s2];
    let ctx = ctx_of(&graph, &block);
    // `checked_add` refuses; a wrapping add would have produced
    // `i64::MIN`, which is not "a small displacement" either but *is* a
    // different, silently wrong answer.
    assert!(matches!(
        match_address(&ctx, s2),
        Err(AddrRefusal::DispOverflow(_)) | Err(AddrRefusal::Unencodable(_))
    ));
}

#[test]
fn small_multiplies_become_two_term_addresses() {
    for (mult, want_scale) in [(3i64, 2u8), (5, 4), (9, 8)] {
        let mut graph = g();
        let x = param(&mut graph, 0);
        let k = konst(&mut graph, mult);
        let mul = bin(&mut graph, IrOp::Mul, x, k);
        let block = vec![k, mul];
        let ctx = ctx_of(&graph, &block);
        let m = match_address(&ctx, mul)
            .unwrap_or_else(|e| panic!("x*{mult} must be an address: {e:?}"));
        assert_eq!(m.addr.base, Some(x));
        assert_eq!(m.addr.index, Some(x));
        assert_eq!(m.addr.scale, want_scale);
        assert_eq!(m.addr.disp, 0);
        assert!(m.addr.check().is_ok());
    }
}

/// `x*4` needs `[x*4]` — no base — which `Mem` cannot express. Refuse
/// rather than emit an operand nobody can lower.
#[test]
fn a_base_less_address_is_refused() {
    let mut graph = g();
    let x = param(&mut graph, 0);
    let k = konst(&mut graph, 4);
    let mul = bin(&mut graph, IrOp::Mul, x, k);
    let block = vec![k, mul];
    let ctx = ctx_of(&graph, &block);
    assert_eq!(
        match_address(&ctx, mul),
        Err(AddrRefusal::NotAnAddress(mul))
    );
    // And the type itself refuses a base-less operand outright.
    let bare = IrAddr {
        base: None,
        index: Some(x),
        scale: 4,
        disp: 0,
    };
    assert_eq!(bare.check(), Err(SelError::NoBaseRegister));
}

#[test]
fn an_illegal_scale_is_refused() {
    let bad = IrAddr {
        base: Some(0),
        index: Some(1),
        scale: 3,
        disp: 0,
    };
    assert_eq!(bad.check(), Err(SelError::BadScale { scale: 3 }));
}

#[test]
fn the_address_walk_is_bounded() {
    let mut graph = g();
    let mut acc = param(&mut graph, 0);
    let mut block = Vec::new();
    for n in 0..(ADDR_MATCH_BUDGET as i64 + 8) {
        let k = konst(&mut graph, n);
        acc = bin(&mut graph, IrOp::Add, acc, k);
        block.push(k);
        block.push(acc);
    }
    let ctx = ctx_of(&graph, &block);
    assert_eq!(match_address(&ctx, acc), Err(AddrRefusal::Budget));
}

// ── LEA versus the two-address ALU form ───────────────────────────────

/// `LEA` is non-destructive, so it wins exactly when the left operand is
/// still live and the ALU form would need a `MOV` first. This is the cost
/// model doing real work: neither instruction is unconditionally better.
#[test]
fn lea_wins_only_when_the_alu_form_would_need_a_copy() {
    // (a) left operand dies here: plain ADD is cheaper.
    let mut graph = g();
    let p = param(&mut graph, 0);
    let q = param(&mut graph, 1);
    let add = bin(&mut graph, IrOp::Add, p, q);
    let block = vec![add];
    let s = select_block(&graph, &block, None, &SelectOptions::default());
    let t = s.tiles.iter().find(|t| t.root == add).expect("root");
    assert_eq!(t.rule, Rule::AluReg, "a dying operand needs no copy");

    // (b) left operand is live afterwards: the copy makes LEA cheaper.
    let mut graph = g();
    let p = param(&mut graph, 0);
    let q = param(&mut graph, 1);
    let add = bin(&mut graph, IrOp::Add, p, q);
    let _keep = second_use(&mut graph, p);
    let block = vec![add];
    let s = select_block(&graph, &block, None, &SelectOptions::default());
    let t = s.tiles.iter().find(|t| t.root == add).expect("root");
    assert_eq!(t.rule, Rule::Lea, "a live operand makes the copy real");
    assert!(matches!(t.insts.as_slice(), [MInst::Lea { .. }]));
}

/// The immediate form wins under frame homing, and loses without it.
///
/// Measured, not assumed: with `frame_homed` off the level-2 encoder took
/// 39 tiles across CratonBenchC2's three phases and `Rule::AluImm` was
/// selected **zero** times — not because the rows were missing (increment 1
/// added them) and not because the constant was folded away, but because
/// `ADD EAX, ECX` is two bytes and `ADD EAX, 7` is three. The cost model
/// prices instructions; under frame homing the register operand also costs
/// a `MOV r64, [RBP-disp8]` that the immediate form does not, and pricing
/// only the instruction hides four bytes and a micro-op.
///
/// The constant is deliberately given a second consumer here, because that
/// is what real code looks like: `ValueUses::single_use` requires
/// `count == 1` AND not pinned, and a safepoint snapshot names almost every
/// live constant — so the tile usually cannot absorb it and the two forms
/// really are competing over one node.
///
/// The exact edit that trips it: drop the `opts.frame_homed` arm from
/// `tiles_alu`'s `extra`. The first assertion flips back to `AluReg`.
#[test]
fn frame_homing_makes_the_immediate_form_win() {
    let build = || {
        let mut graph = g();
        let p = param(&mut graph, 0);
        let k = konst(&mut graph, 7);
        let add = bin(&mut graph, IrOp::Add, p, k);
        // A second consumer for the CONSTANT only, so it is not absorbable
        // and both candidates cover exactly the add — and so the left
        // operand stays single-use, which keeps the two-address copy out of
        // the comparison. This is a test about operand loads, not copies.
        let _other = second_use(&mut graph, k);
        (graph, add)
    };

    let (graph, add) = build();
    let block = vec![add];
    let homed = SelectOptions {
        frame_homed: true,
        ..SelectOptions::default()
    };
    let s = select_block(&graph, &block, None, &homed);
    let t = s.tiles.iter().find(|t| t.root == add).expect("root");
    assert_eq!(
        t.rule,
        Rule::AluImm,
        "the immediate form drops a frame load the register form pays"
    );
    assert_eq!(t.covered.as_slice(), [add], "the constant is still live");

    // And the default is untouched, which is what keeps increments 0 and
    // 1's coverage figures comparable.
    let (graph, add) = build();
    let s = select_block(&graph, &vec![add], None, &SelectOptions::default());
    let t = s.tiles.iter().find(|t| t.root == add).expect("root");
    assert_eq!(t.rule, Rule::AluReg);
}

/// …and under [`SelectOptions::frame_homed`] the copy is *not* real, so the
/// same graph selects the `ADD` again.
///
/// This is the option earning its keep rather than being a preference. The
/// consumer that sets it (`ir_lower`'s level-2 encoder) loads the left
/// operand into the destination register whatever the tile says, so a
/// `MInst::Move` prefix costs nothing and buys nothing — and priced as
/// though it cost something it hands `a + b` an `LEA` a byte longer than
/// the `ADD` it replaced.
///
/// The exact edit that trips it: drop the `!opts.frame_homed &&` from
/// `needs_copy`. Both assertions below flip.
#[test]
fn frame_homing_removes_the_copy_that_makes_lea_win() {
    let mut graph = g();
    let p = param(&mut graph, 0);
    let q = param(&mut graph, 1);
    let add = bin(&mut graph, IrOp::Add, p, q);
    let _keep = second_use(&mut graph, p);
    let block = vec![add];
    let opts = SelectOptions {
        frame_homed: true,
        ..SelectOptions::default()
    };
    let s = select_block(&graph, &block, None, &opts);
    let t = s.tiles.iter().find(|t| t.root == add).expect("root");
    assert_eq!(
        t.rule,
        Rule::AluReg,
        "frame homing means there is no copy to avoid"
    );
    assert!(
        matches!(t.insts.as_slice(), [MInst::AluRR { .. }]),
        "and no `MInst::Move` prefix either: {:?}",
        t.insts
    );
}

/// Ranking is micro-ops first. A three-byte `MOV` plus a three-byte `ADD`
/// is *more* bytes than a four-byte `LEA`, but the point of the ordering is
/// that it would still lose on micro-ops even if it were shorter.
#[test]
fn the_cost_key_ranks_micro_ops_before_bytes() {
    let cheap = SeqCost {
        bytes: 12,
        uops: 1,
        latency: 1,
    };
    let dear = SeqCost {
        bytes: 2,
        uops: 2,
        latency: 1,
    };
    assert!(cheap.key() < dear.key());
    assert_eq!(
        SeqCost::of(Cost::new(3, 1, 1))
            .then(SeqCost::of(Cost::new(3, 1, 1)))
            .key(),
        (2, 6, 2)
    );
}

/// The generic lowering has to be the most expensive thing the model can
/// name, or a rule that covers more nodes would never be preferred.
#[test]
fn the_generic_lowering_is_the_most_expensive_option() {
    let generic = SeqCost::of(GENERIC_COST);
    for i in [
        MInst::AluRR {
            op: Op::Add,
            ty: Ty::I64,
            dst: 0,
            lhs: 1,
            rhs: 2,
        },
        MInst::Lea {
            dst: 0,
            ty: Ty::I64,
            addr: IrAddr {
                base: Some(1),
                index: Some(2),
                scale: 8,
                disp: 64,
            },
        },
        MInst::CmpRR {
            ty: Ty::I64,
            lhs: 0,
            rhs: 1,
        },
    ] {
        assert!(
            i.cost().key() < generic.key(),
            "{i:?} must be cheaper than the generic lowering"
        );
    }
}

// ── compare / branch fusion ───────────────────────────────────────────

/// Build `if (a <cc> b) …` with the compare feeding only the branch.
fn cmp_branch_graph(cc: CmpOp, rhs_zero: bool) -> (Graph, NodeId, NodeId, Vec<NodeId>) {
    let mut graph = g();
    let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
    let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
    let a = graph.add(IrOp::Param(0), IrType::Int, vec![], None);
    let b = if rhs_zero {
        konst_i32(&mut graph, 0)
    } else {
        graph.add(IrOp::Param(1), IrType::Int, vec![], None)
    };
    let cmp = graph.add(IrOp::Cmp(cc), IrType::Int, vec![a, b], None);
    let iff = graph.add(IrOp::If, IrType::Control, vec![ctrl, cmp], None);
    let block = if rhs_zero { vec![b, cmp] } else { vec![cmp] };
    (graph, cmp, iff, block)
}

#[test]
fn a_compare_that_only_feeds_a_branch_is_fused_into_it() {
    let (graph, cmp, iff, block) = cmp_branch_graph(CmpOp::Lt, false);
    let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
    assert!(s.covers(&block), "coverage: {s:?}");
    let t = s.tiles.last().expect("a terminator tile");
    assert_eq!(t.rule, Rule::CmpBranch);
    assert!(t.covered.contains(&cmp), "the compare is consumed");
    assert!(matches!(
        t.insts.as_slice(),
        [MInst::CmpRR { .. }, MInst::Jcc { cc: CmpOp::Lt, .. }]
    ));
    // No SETcc/MOVZX anywhere: that is the whole saving.
    assert!(!s
        .tiles
        .iter()
        .flat_map(|t| t.insts.iter())
        .any(|i| matches!(i, MInst::SetCc { .. })));
}

#[test]
fn a_compare_against_zero_becomes_test() {
    let (graph, cmp, iff, block) = cmp_branch_graph(CmpOp::Ne, true);
    let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
    assert!(s.covers(&block), "coverage: {s:?}");
    let t = s.tiles.last().expect("a terminator tile");
    assert_eq!(t.rule, Rule::TestZeroBranch);
    assert!(t.covered.contains(&cmp));
    assert!(matches!(
        t.insts.as_slice(),
        [
            MInst::TestRR { ty: Ty::I32, .. },
            MInst::Jcc { cc: CmpOp::Ne, .. }
        ]
    ));
    // `TEST r32, r32` is two bytes where `CMP r32, imm8` is three.
    assert_eq!(
        MInst::TestRR {
            ty: Ty::I32,
            reg: 0
        }
        .cost()
        .bytes,
        2
    );
}

/// A zero on the *left* means the operands swapped, so the condition has
/// to be mirrored — not negated. `0 < x` is `x > 0`, never `x >= 0`.
#[test]
fn a_zero_on_the_left_mirrors_the_condition() {
    let mut graph = g();
    let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
    let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
    let zero = konst_i32(&mut graph, 0);
    let x = graph.add(IrOp::Param(0), IrType::Int, vec![], None);
    let cmp = graph.add(IrOp::Cmp(CmpOp::Lt), IrType::Int, vec![zero, x], None);
    let iff = graph.add(IrOp::If, IrType::Control, vec![ctrl, cmp], None);
    let block = vec![zero, cmp];
    let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
    let t = s.tiles.last().expect("terminator");
    assert!(
        matches!(
            t.insts.as_slice(),
            [MInst::TestRR { reg, .. }, MInst::Jcc { cc: CmpOp::Gt, .. }] if *reg == x
        ),
        "0 < x must become TEST x,x / JG, got {:?}",
        t.insts
    );
    // Mirroring is not negation.
    assert_eq!(mirror(CmpOp::Lt), CmpOp::Gt);
    assert_eq!(CmpOp::Lt.negate(), CmpOp::Ge);
}

/// A compare with a second consumer still has to produce its 0/1 value, so
/// the branch may not consume it.
#[test]
fn a_compare_with_a_second_consumer_is_not_fused() {
    let (mut graph, cmp, iff, block) = cmp_branch_graph(CmpOp::Eq, false);
    let _other = graph.add(IrOp::Neg, IrType::Int, vec![cmp], None);

    // Without the encodability gate, the rule that fires is `CMP; SETcc`.
    let lax = select_block(
        &graph,
        &block,
        Some(iff),
        &SelectOptions {
            require_encodable: false,
            fold_loads: false,
            frame_homed: false,
            allocated_division: false,
            allocated_setcc: false,
        },
    );
    let own = lax.tiles.iter().find(|t| t.root == cmp).expect("cmp tile");
    assert_eq!(own.rule, Rule::CmpSetCc);
    assert!(own.insts.iter().any(|i| matches!(i, MInst::SetCc { .. })));

    // With it, `SETcc` has no row (its 8-bit destination needs a REX rule
    // the table does not state), so the compare falls back to the generic
    // lowering — which is byte-for-byte what `ir_lower` emits today.
    let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
    assert!(s.covers(&block));
    let term = s.tiles.last().expect("terminator");
    assert_eq!(term.rule, Rule::TestBranch);
    assert!(!term.covered.contains(&cmp));
    let own = s.tiles.iter().find(|t| t.root == cmp).expect("cmp tile");
    assert_eq!(own.rule, Rule::Generic);
    assert!(s.notes.iter().any(|n| matches!(
        n,
        Note::Unencodable {
            rule: Rule::CmpSetCc,
            ..
        }
    )));
}

/// A compare the scheduler put in another block cannot be fused: the
/// instructions in between would have destroyed the flags.
#[test]
fn an_out_of_block_compare_is_not_fused() {
    let (graph, cmp, iff, _block) = cmp_branch_graph(CmpOp::Eq, false);
    let block: Vec<NodeId> = Vec::new();
    let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
    let term = s.tiles.last().expect("terminator");
    assert_eq!(term.rule, Rule::TestBranch);
    assert!(!term.covered.contains(&cmp));
}

// ── the load-folding gate ─────────────────────────────────────────────

/// `[ctrl, mem, base, offset]` load / `[ctrl, mem, base, offset, value]`
/// store over one base, with constant offsets so the alias model can prove
/// (or refuse) disjointness.
struct MemFixture {
    graph: Graph,
    load: NodeId,
    store: NodeId,
    add: NodeId,
    block: Vec<NodeId>,
}

fn mem_fixture(load_off: i64, store_off: i64) -> MemFixture {
    let mut graph = g();
    let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
    let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(IrOp::Proj(1), IrType::Memory, vec![start], None);
    let base = graph.add(IrOp::Param(0), IrType::Ref, vec![], None);
    let lo = graph.add(IrOp::Const(load_off), IrType::Int, vec![], None);
    let so = graph.add(IrOp::Const(store_off), IrType::Int, vec![], None);
    let val = graph.add(IrOp::Param(1), IrType::Int, vec![], None);
    let load = graph.add(
        IrOp::Load(MemKind::Int),
        IrType::Int,
        vec![ctrl, mem, base, lo],
        None,
    );
    // The builder threads the token through the load, so the store's
    // slot-1 edge names it. That edge must NOT count as a value use.
    let store = graph.add(
        IrOp::Store(MemKind::Int),
        IrType::Void,
        vec![ctrl, load, base, so, val],
        None,
    );
    let other = graph.add(IrOp::Param(2), IrType::Int, vec![], None);
    let add = graph.add(IrOp::Add, IrType::Int, vec![other, load], None);
    let block = vec![load, store, add];
    MemFixture {
        graph,
        load,
        store,
        add,
        block,
    }
}

#[test]
fn the_memory_token_edge_is_not_a_value_use() {
    let f = mem_fixture(1, 1);
    let uses = ValueUses::of(&f.graph);
    assert_eq!(
        uses.count(f.load),
        1,
        "the store's token edge must not count; only the Add reads the value"
    );
    assert!(uses.single_use(f.load));
}

/// The defect this gate exists to prevent: folding the load into the Add
/// moves it past the store, so it would read the value the store *just
/// wrote* instead of the one that was there.
#[test]
fn a_load_is_not_folded_across_an_aliasing_store() {
    let f = mem_fixture(1, 1);
    let ctx = ctx_of(&f.graph, &f.block);
    assert_eq!(
        may_fold_load(&ctx, f.load, f.add),
        Err(FoldRefusal::Intervening {
            between: f.store,
            reason: ReorderBlock::MayAlias
        })
    );
    let opts = SelectOptions {
        require_encodable: false,
        fold_loads: true,
        frame_homed: false,
        allocated_division: false,
        allocated_setcc: false,
    };
    let s = select_block(&f.graph, &f.block, None, &opts);
    assert!(
        !s.matched().any(|t| t.rule == Rule::AluFoldedLoad),
        "no fold may survive an aliasing store"
    );
    assert!(s.covers(&f.block));
}

/// A store the alias model proves disjoint does not block the fold.
#[test]
fn a_load_is_folded_across_a_provably_disjoint_store() {
    let f = mem_fixture(1, 2);
    let ctx = ctx_of(&f.graph, &f.block);
    assert_eq!(may_fold_load(&ctx, f.load, f.add), Ok(()));
    let opts = SelectOptions {
        require_encodable: false,
        fold_loads: true,
        frame_homed: false,
        allocated_division: false,
        allocated_setcc: false,
    };
    let s = select_block(&f.graph, &f.block, None, &opts);
    let t = s.tiles.iter().find(|t| t.root == f.add).expect("add tile");
    assert_eq!(t.rule, Rule::AluFoldedLoad);
    assert!(t.covered.contains(&f.load));
    assert!(s.covers(&f.block), "coverage: {s:?}");
    // The folded load must not also be emitted on its own.
    assert!(
        !s.tiles.iter().any(|t| t.root == f.load),
        "the load may not be a tile root as well as folded"
    );
}

#[test]
fn a_multiply_used_load_is_not_folded() {
    let mut f = mem_fixture(1, 2);
    let _second = f.graph.add(IrOp::Neg, IrType::Int, vec![f.load], None);
    let ctx = ctx_of(&f.graph, &f.block);
    assert_eq!(
        may_fold_load(&ctx, f.load, f.add),
        Err(FoldRefusal::MultipleUses {
            load: f.load,
            uses: 2
        })
    );
}

#[test]
fn a_safepoint_pinned_load_is_not_folded() {
    let mut f = mem_fixture(1, 2);
    f.graph.safepoints.push(SafepointSnapshot::new(
        0,
        vec![f.load],
        Vec::new(),
        Vec::new(),
    ));
    let ctx = ctx_of(&f.graph, &f.block);
    assert_eq!(
        may_fold_load(&ctx, f.load, f.add),
        Err(FoldRefusal::SafepointPinned(f.load))
    );
}

#[test]
fn every_snapshot_domain_pins_its_held_value() {
    for domain in 0..3 {
        let mut graph = g();
        let held = graph.add(IrOp::Param(0), IrType::Ref, vec![], None);
        let length = graph.add(IrOp::ArrayLength, IrType::Int,
            vec![NO_NODE, NO_NODE, held], None);
        let mut domains = [Vec::new(), Vec::new(), Vec::new()];
        domains[domain].push(held);
        let [locals, stack, monitors] = domains;
        graph.safepoints.push(SafepointSnapshot::new(0, locals, stack, monitors));
        let uses = ValueUses::of(&graph);
        assert_eq!(uses.count(held), 1);
        assert!(uses.is_pinned(held), "snapshot domain {domain} must pin its held object");
        assert!(!ctx_of(&graph, &[held, length]).absorbable(held));
    }
}

#[test]
fn a_load_after_its_user_is_not_folded() {
    let f = mem_fixture(1, 2);
    // Present the block in an order where the load comes last.
    let block = vec![f.store, f.add, f.load];
    let ctx = ctx_of(&f.graph, &block);
    assert_eq!(
        may_fold_load(&ctx, f.load, f.add),
        Err(FoldRefusal::NotBefore {
            load: f.load,
            user: f.add
        })
    );
}

#[test]
fn a_load_outside_the_block_is_not_folded() {
    let f = mem_fixture(1, 2);
    let block = vec![f.add];
    let ctx = ctx_of(&f.graph, &block);
    assert_eq!(
        may_fold_load(&ctx, f.load, f.add),
        Err(FoldRefusal::NotInBlock(f.load))
    );
}

/// A call between the load and its user is opaque: it may write anything.
#[test]
fn a_load_is_not_folded_across_a_call() {
    let mut f = mem_fixture(1, 2);
    let ctrl = f.graph.nodes[f.load as usize].inputs[0];
    let call = f.graph.add(
        IrOp::Call { info_ptr: 0 },
        IrType::Int,
        vec![ctrl, f.load],
        None,
    );
    let block = vec![f.load, call, f.add];
    let ctx = ctx_of(&f.graph, &block);
    assert!(matches!(
        may_fold_load(&ctx, f.load, f.add),
        Err(FoldRefusal::Intervening { between, .. }) if between == call
    ));
}

// ── coverage and fail-closed behaviour ────────────────────────────────

/// The invariant everything else rests on. A node covered twice is
/// computed twice; a node covered zero times is a *dropped instruction* —
/// the failure mode a catch-all `_ => {}` produces.
#[test]
fn every_block_node_is_covered_exactly_once() {
    let mut graph = g();
    let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
    let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
    let p = param(&mut graph, 0);
    let q = param(&mut graph, 1);
    let k2 = konst(&mut graph, 2);
    let shl = bin(&mut graph, IrOp::Shl, q, k2);
    let k8 = konst(&mut graph, 8);
    let a1 = bin(&mut graph, IrOp::Add, p, shl);
    let a2 = bin(&mut graph, IrOp::Add, a1, k8);
    let k5 = konst(&mut graph, 5);
    let and = bin(&mut graph, IrOp::And, a2, k5);
    let d = graph.add(IrOp::Div, IrType::Long, vec![and, p], None);
    let zero = konst(&mut graph, 0);
    let cmp = graph.add(IrOp::Cmp(CmpOp::Ne), IrType::Int, vec![d, zero], None);
    let iff = graph.add(IrOp::If, IrType::Control, vec![ctrl, cmp], None);
    let block = vec![k2, shl, k8, a1, a2, k5, and, d, zero, cmp];
    let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
    assert!(s.covers(&block), "coverage failed: {s:#?}");
    // No node appears in two tiles.
    let mut all: Vec<NodeId> = s
        .tiles
        .iter()
        .flat_map(|t| t.covered.iter().copied())
        .collect();
    let before = all.len();
    all.sort_unstable();
    all.dedup();
    assert_eq!(before, all.len(), "a node was covered twice");
}

/// An op no rule matches must come back as a tile that *names* it.
#[test]
fn an_unmatched_node_becomes_a_generic_tile_naming_it() {
    let mut graph = g();
    let p = param(&mut graph, 0);
    let q = param(&mut graph, 1);
    let d = graph.add(IrOp::Div, IrType::Long, vec![p, q], None);
    let block = vec![d];
    let s = select_block(&graph, &block, None, &SelectOptions::default());
    assert_eq!(s.tiles.len(), 1);
    assert_eq!(s.tiles[0].rule, Rule::Generic);
    assert_eq!(s.tiles[0].insts, vec![MInst::Generic { node: d }]);
    assert!(s.covers(&block));
}

/// A `MonitorEnter` must never vanish. This is the shape of the bug the
/// brief names: `lower_data_node`'s catch-all meant an unguarded monitor
/// op compiled to nothing and a lock was dropped.
#[test]
fn an_unmatched_monitor_op_is_never_silently_dropped() {
    let mut graph = g();
    let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
    let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
    let mem = graph.add(IrOp::Proj(1), IrType::Memory, vec![start], None);
    let obj = graph.add(IrOp::Param(0), IrType::Ref, vec![], None);
    let enter = graph.add(
        IrOp::MonitorEnter,
        IrType::Memory,
        vec![ctrl, mem, obj],
        None,
    );
    let exit = graph.add(
        IrOp::MonitorExit,
        IrType::Memory,
        vec![ctrl, enter, obj],
        None,
    );
    let block = vec![enter, exit];
    let s = select_block(&graph, &block, None, &SelectOptions::default());
    assert!(s.covers(&block), "both monitor ops must be covered");
    for id in [enter, exit] {
        let t = s.tiles.iter().find(|t| t.root == id).expect("a tile");
        assert_eq!(t.rule, Rule::Generic);
        assert_eq!(t.insts, vec![MInst::Generic { node: id }]);
    }
}

/// A terminator always gets a tile. A branch selection declined to cover
/// is a branch nobody emits — the one failure this design must not have.
#[test]
fn a_terminator_always_gets_a_tile() {
    // (a) an `If` whose compare is fusable.
    let (graph, _cmp, iff, block) = cmp_branch_graph(CmpOp::Le, false);
    let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
    assert!(matches!(
        s.tiles.last().map(|t| t.root),
        Some(r) if r == iff
    ));

    // (b) a `Return`, which no rule matches: still a tile, and it NAMES
    // the node so the caller knows it has to emit the epilogue.
    let mut graph = g();
    let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
    let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
    let ret = graph.add(IrOp::Return, IrType::Control, vec![ctrl], None);
    let s = select_block(&graph, &[], Some(ret), &SelectOptions::default());
    assert_eq!(s.tiles.len(), 1);
    assert_eq!(s.tiles[0].rule, Rule::Generic);
    assert_eq!(s.tiles[0].insts, vec![MInst::Generic { node: ret }]);
}

/// A tile the pattern table cannot encode is discarded, not emitted.
#[test]
fn a_tile_the_table_cannot_encode_is_discarded_and_reported() {
    let mut graph = g();
    let p = param(&mut graph, 0);
    let k = konst(&mut graph, 5);
    // A comparison whose 0/1 result is read needs SETcc, which has no row.
    let x = graph.add(IrOp::Cmp(CmpOp::Eq), IrType::Int, vec![p, k], None);
    let block = vec![k, x];

    let lax = select_block(
        &graph,
        &block,
        None,
        &SelectOptions {
            require_encodable: false,
            fold_loads: false,
            frame_homed: false,
            allocated_division: false,
            allocated_setcc: false,
        },
    );
    assert_eq!(
        lax.tiles.iter().find(|t| t.root == x).map(|t| t.rule),
        Some(Rule::CmpSetCc),
        "without the gate the materialized compare is what the rules pick"
    );

    let strict = select_block(&graph, &block, None, &SelectOptions::default());
    let t = strict.tiles.iter().find(|t| t.root == x).expect("root");
    assert_eq!(t.rule, Rule::Generic, "the gate must fall back to Generic");
    assert!(
        strict.notes.iter().any(|n| matches!(
            n,
            Note::Unencodable {
                rule: Rule::CmpSetCc,
                ..
            }
        )),
        "the discard must be reported: {:?}",
        strict.notes
    );
    assert!(strict.covers(&block));
}

/// Under the default options every non-generic instruction the selector
/// chooses has a proven encoding. This is what makes `require_encodable`
/// a gate rather than a comment.
#[test]
fn every_selected_instruction_encodes_under_the_default_options() {
    let mut graph = g();
    let start = graph.add(IrOp::Start, IrType::Control, vec![], None);
    let ctrl = graph.add(IrOp::Proj(0), IrType::Control, vec![start], None);
    let p = param(&mut graph, 0);
    let q = param(&mut graph, 1);
    let k2 = konst(&mut graph, 2);
    let shl = bin(&mut graph, IrOp::Shl, q, k2);
    let a1 = bin(&mut graph, IrOp::Add, p, shl);
    let k = konst(&mut graph, 40);
    let a2 = bin(&mut graph, IrOp::Add, a1, k);
    let _live = second_use(&mut graph, a2);
    let zero = konst(&mut graph, 0);
    let cmp = graph.add(IrOp::Cmp(CmpOp::Gt), IrType::Int, vec![a2, zero], None);
    let iff = graph.add(IrOp::If, IrType::Control, vec![ctrl, cmp], None);
    let block = vec![k2, shl, a1, k, a2, zero, cmp];
    let s = select_block(&graph, &block, Some(iff), &SelectOptions::default());
    assert!(s.covers(&block), "coverage: {s:#?}");
    for t in s.matched() {
        for i in &t.insts {
            let e = i
                .probe()
                .unwrap_or_else(|e| panic!("{i:?} in {:?} does not encode: {e}", t.rule));
            assert!(!e.bytes.is_empty(), "{i:?} encoded to nothing");
        }
    }
}

/// Selection is a function of its input: the same graph and block must
/// produce the same tiling every time, or a regression cannot be bisected.
#[test]
fn selection_is_deterministic() {
    let mut graph = g();
    let p = param(&mut graph, 0);
    let q = param(&mut graph, 1);
    let k2 = konst(&mut graph, 2);
    let shl = bin(&mut graph, IrOp::Shl, q, k2);
    let a1 = bin(&mut graph, IrOp::Add, p, shl);
    let block = vec![k2, shl, a1];
    let a = select_block(&graph, &block, None, &SelectOptions::default());
    let b = select_block(&graph, &block, None, &SelectOptions::default());
    assert_eq!(a, b);
}

/// Every 32-bit immediate row reproduces the `x64.rs` byte literal it
/// names, exactly.
///
/// The table's one trustworthy property is that a row is anchored to
/// hand-written code it matches byte-for-byte; eight rows added on the
/// strength of a coverage measurement are eight chances to weaken it. Each
/// literal below is copied from the constant-folding fast path in `x64.rs`
/// (the `iadd`/`isub`/`iand`/`ior`/`ixor`/`if_icmp` const arms), with EAX
/// as the destination — which is what those arms use, and why no REX
/// prefix appears.
///
/// The exact edit that trips it: change any `RegF::Ext(n)` above. The
/// `/n` digit is the opcode extension that distinguishes `ADD` from `SUB`
/// from `AND` in the shared `0x83` group, and getting it wrong produces a
/// valid instruction that computes something else entirely.
#[test]
fn the_32bit_immediate_rows_reproduce_the_x64_byte_literals() {
    // `x64.rs` constant-folding fast path, EAX destination — which is why
    // no REX prefix appears in any of these.
    assert_eq!(
        sel(&gpr_imm(Op::Add, Ty::I32, RAX, 7)),
        vec![0x83, 0xC0, 0x07]
    );
    assert_eq!(
        sel(&gpr_imm(Op::Add, Ty::I32, RAX, 100_000)),
        vec![0x81, 0xC0, 0xA0, 0x86, 0x01, 0x00]
    );
    assert_eq!(
        sel(&gpr_imm(Op::Sub, Ty::I32, RAX, 7)),
        vec![0x83, 0xE8, 0x07]
    );
    assert_eq!(
        sel(&gpr_imm(Op::Sub, Ty::I32, RAX, 100_000)),
        vec![0x81, 0xE8, 0xA0, 0x86, 0x01, 0x00]
    );
    assert_eq!(
        sel(&gpr_imm(Op::And, Ty::I32, RAX, 7)),
        vec![0x83, 0xE0, 0x07]
    );
    assert_eq!(
        sel(&gpr_imm(Op::Or, Ty::I32, RAX, 7)),
        vec![0x83, 0xC8, 0x07]
    );
    assert_eq!(
        sel(&gpr_imm(Op::Xor, Ty::I32, RAX, 7)),
        vec![0x83, 0xF0, 0x07]
    );
    assert_eq!(
        sel(&gpr_imm(Op::Cmp, Ty::I32, RAX, 7)),
        vec![0x83, 0xF8, 0x07]
    );
}

/// The three `CMP r, imm` rows this round added, each against the byte
/// literal it is anchored to.
///
/// The gap they close is not cosmetic: `MInst::CmpRI` named a row only at
/// `(I32, Imm8)`, so under `SelectOptions::require_encodable` every compare
/// against a constant outside `-128..=127`, and every `long`/reference
/// compare against any non-zero constant, was DISCARDED — the tile fell
/// back to `Rule::Generic` and the measurement read as "the rule never
/// fires".
///
/// The exact edit that trips it: change any `RegF::Ext(7)` on the three new
/// rows. `/7` is what makes the shared `0x81`/`0x83` group mean `CMP`
/// rather than `ADD` (`/0`) or `SUB` (`/5`) — a valid instruction that
/// computes something else and then branches on it.
#[test]
fn the_new_cmp_immediate_rows_reproduce_their_byte_literals() {
    // `x64/simd.rs` emit_strip_clamp: `CMP R8D, MAX_BULK_BYTE_LOOP_SPAN`.
    assert_eq!(
        named(
            "cmp_r32_imm32",
            &Args {
                dst: R8,
                imm: 100_000,
                ..Args::default()
            }
        ),
        vec![0x41, 0x81, 0xF8, 0xA0, 0x86, 0x01, 0x00],
    );
    // `x64/arith.rs` emit_fp_to_int_nan_fixup: `CMP RAX, 1`.
    assert_eq!(
        named(
            "cmp_r64_imm8",
            &Args {
                dst: RAX,
                imm: 1,
                ..Args::default()
            }
        ),
        vec![0x48, 0x83, 0xF8, 0x01],
    );
    // `x64/objects.rs` ZGC TLAB guard: `CMP R8, 512`.
    assert_eq!(
        named(
            "cmp_r64_imm32",
            &Args {
                dst: R8,
                imm: 512,
                ..Args::default()
            }
        ),
        vec![0x49, 0x81, 0xF8, 0x00, 0x02, 0x00, 0x00],
    );
    // The smallest form still wins where both apply, so no existing call
    // site's bytes move.
    assert_eq!(
        sel(&gpr_imm(Op::Cmp, Ty::I64, RAX, 1)),
        vec![0x48, 0x83, 0xF8, 0x01],
    );
    assert_eq!(
        sel(&gpr_imm(Op::Cmp, Ty::I32, RAX, 100_000)),
        vec![0x81, 0xF8, 0xA0, 0x86, 0x01, 0x00],
    );
    // And the tiles that name them really encode.
    for (ty, form, imm) in [
        (Ty::I32, ImmForm::Imm8, 7i64),
        (Ty::I32, ImmForm::Imm32, 100_000),
        (Ty::I64, ImmForm::Imm8, 1),
        (Ty::I64, ImmForm::Imm32, 512),
    ] {
        let inst = MInst::CmpRI {
            ty,
            lhs: 0,
            imm,
            form,
        };
        let name = inst
            .pattern_name()
            .unwrap_or_else(|| panic!("{ty:?}/{form:?} has a row but no mapping"));
        assert!(pattern(name).is_some(), "`{name}` is not in PATTERNS");
        inst.probe()
            .unwrap_or_else(|e| panic!("{ty:?}/{form:?}: {e:?}"));
    }
}

/// `lea_r32_m` reproduces the three byte literals `emit_imul_const` emits.
///
/// `Rule::Lea` fired **zero** times on 850 real Spring Boot compiles
/// because `tile_lea` refused every `Ty::I32` root — Java arithmetic is
/// 32-bit and the table had only the REX.W form. This is the anchor for the
/// row that unblocked it: `x64/arith.rs:790`, `:797`, `:804` already write
/// exactly these bytes for `imul` by 3, 5 and 9.
///
/// The exact edit that trips it: set `rex_w: true` on the row, or change
/// `RexMode::OnDemand` to `RexMode::Always`. Either adds a `0x48` and the
/// row stops being the literal it claims.
#[test]
fn the_32bit_lea_row_reproduces_the_imul_const_byte_literals() {
    for (scale, want) in [
        (2u8, vec![0x8Du8, 0x04, 0x40]),
        (4, vec![0x8D, 0x04, 0x80]),
        (8, vec![0x8D, 0x04, 0xC0]),
    ] {
        let mem = Mem {
            base: RAX,
            index: Some(Index { reg: RAX, scale }),
            disp: 0,
            force_disp32: false,
        };
        let got = sel(&Req::new(
            Op::Lea,
            Ty::I32,
            Operand::Gpr(RAX),
            Operand::Mem(mem),
        ));
        assert_eq!(got, want, "LEA EAX, [RAX + RAX*{scale}]");
    }
}

/// An `int` add/shift/multiply tree becomes one `LEA`, which is the whole
/// point of the row above: before it, `tile_lea` returned `None` for every
/// `IrType::Int` root and the rule was dead on real code.
#[test]
fn an_int_address_tree_now_tiles_as_a_lea() {
    let mut graph = g();
    let p = graph.add(IrOp::Param(0), IrType::Int, vec![], None);
    let q = graph.add(IrOp::Param(1), IrType::Int, vec![], None);
    let k = konst_i32(&mut graph, 2);
    let shl = graph.add(IrOp::Shl, IrType::Int, vec![q, k], None);
    let add = graph.add(IrOp::Add, IrType::Int, vec![p, shl], None);
    let block = vec![k, shl, add];
    let s = select_block(&graph, &block, None, &SelectOptions::default());
    assert!(s.covers(&block), "coverage: {s:?}");
    let root = s.tiles.iter().find(|t| t.root == add).expect("root");
    assert_eq!(root.rule, Rule::Lea, "an int address tree must fold");
    match root.insts.as_slice() {
        [MInst::Lea { ty, addr, .. }] => {
            assert_eq!(*ty, Ty::I32, "an int root must select the 32-bit form");
            assert_eq!(addr.scale, 4);
        }
        other => panic!("expected one LEA, got {other:?}"),
    }
    // And the tile the selector produced is one the table can encode —
    // a rule that fires but cannot encode is discarded by
    // `require_encodable` and reads as "fired zero times" all over again.
    for i in &root.insts {
        i.probe().unwrap_or_else(|e| panic!("{i:?}: {e:?}"));
    }
}

/// The new rows are reachable through `MInst::probe`, not merely present.
///
/// A row the table has but `pattern_name` cannot name is a row
/// `SelectOptions::require_encodable` still discards — which is the state
/// the whole 850-method measurement was taken in. Both halves or neither.
#[test]
fn a_32bit_immediate_tile_is_encodable() {
    for (op, form, imm) in [
        (Op::Add, ImmForm::Imm8, 7i64),
        (Op::Add, ImmForm::Imm32, 100_000),
        (Op::Sub, ImmForm::Imm8, 7),
        (Op::And, ImmForm::Imm8, 7),
        (Op::Or, ImmForm::Imm8, 7),
        (Op::Xor, ImmForm::Imm8, 7),
    ] {
        let inst = MInst::AluRI {
            op,
            ty: Ty::I32,
            dst: 0,
            lhs: 1,
            imm,
            form,
        };
        assert!(
            inst.pattern_name().is_some(),
            "{op:?}/{form:?} at I32 has a row but no `pattern_name` mapping"
        );
        inst.probe()
            .unwrap_or_else(|e| panic!("{op:?}/{form:?} at I32: {e:?}"));
    }
}

/// The instructions the table cannot encode yet, pinned so the gap is a
/// fact rather than a surprise. Shrinking this list is the next wave's
/// work; growing it silently is what this test prevents.
#[test]
fn the_rows_the_selector_still_lacks_are_exactly_these() {
    let missing: Vec<&'static str> = [
        MInst::AluRM {
            op: Op::Add,
            ty: Ty::I64,
            dst: 0,
            lhs: 1,
            addr: AddrSource::Opaque(2),
            load: 2,
        },
        MInst::SetCc {
            dst: 0,
            cc: CmpOp::Eq,
        },
    ]
    .iter()
    .filter(|i| i.pattern_name().is_none())
    .map(|_| "unencodable")
    .collect();
    assert_eq!(
        missing.len(),
        2,
        "opaque field addresses and materialized SETcc still need lowering support"
    );
    // And the ones that DO have rows really resolve to a row.
    for i in [
        MInst::AluRR {
            op: Op::Add,
            ty: Ty::I64,
            dst: 0,
            lhs: 1,
            rhs: 2,
        },
        MInst::Move {
            dst: 0,
            ty: Ty::I64,
            src: 1,
        },
        MInst::Lea {
            dst: 0,
            ty: Ty::I64,
            addr: IrAddr {
                base: Some(1),
                index: None,
                scale: 1,
                disp: 0,
            },
        },
        MInst::Lea {
            dst: 0,
            ty: Ty::I32,
            addr: IrAddr {
                base: Some(1),
                index: Some(1),
                scale: 2,
                disp: 0,
            },
        },
        MInst::TestRR {
            ty: Ty::I32,
            reg: 0,
        },
        MInst::Jcc {
            cc: CmpOp::Ne,
            at: 0,
        },
    ] {
        let name = i.pattern_name().unwrap_or_else(|| panic!("{i:?}"));
        assert!(pattern(name).is_some(), "`{name}` is not in PATTERNS");
        assert!(i.probe().is_ok(), "{i:?} must encode");
    }
}
