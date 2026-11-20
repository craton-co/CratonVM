#[allow(unused_imports)]
use super::*;

impl<'a> Lowerer<'a> {
    /// The element-wise vector pre-loop on the entry edge `pred_block ->
    /// succ_block`, when [`crate::ir_vector_loop::plan_vector_loops`] admitted
    /// that loop (round 16 matrix residual, MX16-5). Emitted after the edge's
    /// phi copies, so the induction phi holds its entry value, and before the
    /// jump into the header; see the module doc for the shape and why it is
    /// exact. Emits nothing for every other edge.
    ///
    /// Clobbers RAX, RCX, RDX, R8-R11, the flags, XMM0/XMM1 and the vector
    /// pool (`xmm_roles::VECTOR_REGION_MAX`, as the frame may use it). None of
    /// them holds a value across a block boundary: the GP residency file is
    /// callee-saved (`IR_LOWER_LS_GPRS`), the FP file is XMM2-XMM7, and a carry
    /// never spans a block. `VZEROUPPER` closes the 256-bit region, so the
    /// legacy-SSE code after it pays no transition.
    pub(in crate::ir_lower) fn emit_vector_preloop(&mut self, pred_block: usize, succ_block: usize) {
        use crate::ir_vector_loop as vl;
        if !crate::ir_vector_loop::ir_vector_loop_enabled() {
            return;
        }
        if self.vec_plans.is_none() {
            let graph = self.graph;
            let schedule = self.schedule;
            let (plans, refusals) =
                vl::plan_vector_loops(graph, schedule, &|c| ctrl_block_of(graph, schedule, c));
            if false {
                for p in &plans {
                    eprintln!(
                        "[ir-vec] {} loop at bci {:?}: {:?} x{} lanes, {} arrays, {} invariants{}{}{}",
                        self.graph.dbg_label,
                        p.header_bci,
                        p.kind,
                        p.lanes(),
                        p.bases.len(),
                        p.invariants.len(),
                        match (p.reduce.map(|r| r.op), p.scan.as_ref()) {
                            (_, Some(s)) if s.tests.len() > 1 => ", scan (multi-compare)",
                            (_, Some(_)) => ", scan",
                            (Some(vl::RedOp::Min | vl::RedOp::Max), _) => ", min/max reduction",
                            (Some(_), _) => ", reduction",
                            (None, None) => "",
                        },
                        match p.widen {
                            Some(w) => format!(", widened from {w:?}"),
                            None => String::new(),
                        },
                        // j27 VEC27-1 triage: a segment (scoped) access.
                        match p.scoped {
                            Some(s) if s.native => ", scoped native",
                            Some(_) => ", scoped heap",
                            None => "",
                        }
                    );
                }
                for (h, why) in &refusals {
                    let bci = self
                        .graph
                        .nodes
                        .get(self.schedule.blocks[*h].ctrl as usize)
                        .and_then(|n| n.bytecode_pc);
                    eprintln!(
                        "[ir-vec] {} loop at bci {bci:?} refused: {why}",
                        self.graph.dbg_label
                    );
                }
            }
            self.vec_plans = Some(plans);
        }
        let Some(plan) = self
            .vec_plans
            .as_ref()
            .and_then(|ps| {
                ps.iter()
                    .find(|p| p.entry_pred == pred_block && p.header == succ_block)
            })
            .cloned()
        else {
            return;
        };
        // Refused, too, with a `--gpu` input-cache barrier armed (every scalar
        // primitive store marks its bucket, a vector store would not) or with
        // a split plan installed (a value's register at the header is then its
        // layout predecessor's). Scoped plans have one base, leaving RCX free
        // for the owed-cycle poll address.
        let owed_latch = self.direct_helpers.gen_owed_poll_flag_addr;
        if !crate::x64::has_avx2()
            || !self.ls_edge_copies.is_empty()
            || !self.ls_block_start.is_empty()
            || (owed_latch != 0 && plan.scoped.is_none())
            || crate::gpu_barrier::barrier_bytes().is_some()
        {
            return self.vec_declined(&plan, "no AVX2, a split plan, an owed poll or a GPU barrier");
        }
        // Where `j` goes back: its home word when it has a live one, and its
        // register when it has one. A phi with neither has nowhere to be told.
        let iv = plan.iv;
        let home_live = self.node_slot.get(iv as usize).copied().flatten().is_some()
            && !self.home_dropped.get(iv as usize).copied().unwrap_or(false);
        let iv_reg = self.assigned_gpr(iv);
        if !home_live && iv_reg.is_none() {
            return self.vec_declined(&plan, "the induction has no home and no register");
        }
        // A reduction (r21 vector, VEC21-4): the accumulator phi is told the
        // same way, so it needs a home or a register too.
        let acc_phi = plan.reduce.map(|r| r.acc);
        let red_op = plan.reduce.map(|r| r.op);
        // VEC21-5: an FP min/max accumulator lives in the FP file (its home
        // word, and XMM2-XMM7 when resident), and needs a NaN-seen register.
        let ordered_fp = plan.ordered_fp();
        let fp_acc =
            plan.reduce.is_some() && matches!(plan.kind, vl::VecKind::Double | vl::VecKind::Float);
        let (acc_home_live, acc_reg, acc_xreg) = match acc_phi {
            Some(a) => (
                self.node_slot.get(a as usize).copied().flatten().is_some()
                    && !self.home_dropped.get(a as usize).copied().unwrap_or(false),
                if fp_acc { None } else { self.assigned_gpr(a) },
                if fp_acc { self.assigned_xmm(a) } else { None },
            ),
            None => (false, None, None),
        };
        if acc_phi.is_some() && !acc_home_live && acc_reg.is_none() && acc_xreg.is_none() {
            return self.vec_declined(&plan, "the accumulator has no home and no register");
        }
        // The FP write-back goes through XMM0: a resident register must be
        // one of the FP file, never the vector pool or the value tier.
        if acc_xreg.is_some_and(|r| r == XMM0 || r == XMM1 || r >= 8) {
            return self.vec_declined(&plan, "the FP accumulator is resident outside the FP file");
        }
        // The registers: XMM0/XMM1 and the encodable vector pool. Invariants
        // take the top of it, a reduction's accumulator the next one down,
        // temporaries the rest (at least one: the fold's scratch).
        let mut pool: Vec<u8> = vec![XMM0, XMM1];
        pool.extend(
            crate::regalloc::xmm_roles::VECTOR_REGION_MAX
                .iter()
                .copied()
                .filter(|&r| {
                    // VEC21-7: a frame that saves XMM8-XMM11 may hand them
                    // out on Windows too (`vector_saved_fp` marked them saved).
                    crate::regalloc::xmm_roles::vector_pool_is_encodable(r, self.saved_xmm_file)
                }),
        );
        let acc_regs = usize::from(acc_phi.is_some()) + usize::from(fp_acc && !ordered_fp);
        // VEC21-6's shift counts and VEC21-2's narrowing-store shuffle control
        // take registers after the invariants'.
        let narrow_mask = vl::narrow_store_mask(&plan);
        // JIT round 24 B1: an iota plan's running and step registers last.
        let pinned_regs = plan.invariants.len()
            + plan.shift_counts.len()
            + usize::from(narrow_mask.is_some())
            + 2 * usize::from(plan.iota);
        if pinned_regs + acc_regs >= pool.len() {
            return self.vec_declined(&plan, "more invariants than vector registers");
        }
        let inv_regs: Vec<u8> = pool.split_off(pool.len() - pinned_regs);
        let acc_vreg: Option<u8> = if acc_phi.is_some() { pool.pop() } else { None };
        let nan_vreg: Option<u8> = if fp_acc && !ordered_fp { pool.pop() } else { None };
        let regs = vl::StepRegs {
            temps: &pool,
            inv_regs: &inv_regs,
            acc: acc_vreg,
            nan: nan_vreg,
        };
        let fold: Option<Vec<u8>> = match (acc_vreg, red_op) {
            (Some(a), _) if ordered_fp => Some(vl::scalar_vector_move(XMM0,a)),
            (Some(a), Some(op)) if op.is_minmax() => {
                let s1 = pool.get(1).copied().unwrap_or(pool[0]);
                match vl::minmax_fold_code(plan.kind, op == vl::RedOp::Max, a, pool[0], s1) {
                    Some(code) => Some(code),
                    None => return self.vec_declined(&plan, "no registers for the min/max fold"),
                }
            }
            (Some(a), _) => match vl::fold_code(plan.kind, a, pool[0]) {
                Some(code) => Some(code),
                None => return self.vec_declined(&plan, "no registers for the fold"),
            },
            (None, _) => None,
        };
        // 32 bytes of each array per step; a widened scan (L8-4) reads
        // `lanes * element` bytes (8 or 16).
        let step_bytes = plan.step_bytes();
        let mut steps: Vec<Vec<u8>> = Vec::with_capacity(plan.unroll());
        for u in 0..plan.unroll() {
            // Cast: HEADER_SIZE + 3 * 32 <= 127 (asserted by the conversion).
            let Ok(disp) = i8::try_from(HEADER_SIZE as i32 + u as i32 * step_bytes) else {
                return self.vec_declined(&plan, "a step displacement past a byte");
            };
            match vl::step_code(&plan, &regs, disp) {
                Some(code) => steps.push(code),
                None => {
                    return self.vec_declined(&plan, "the step needs more vector temporaries");
                }
            }
        }
        let lanes = plan.lanes();
        // A wide fill with less than one step left stores it under a mask
        // (after the base tests and the broadcast) instead of leaving for the
        // polling scalar loop: `Matcher.search`'s 2-int `groups` reset.
        // ARRAY_DATA_DISP8 is disp8-checked at compile time, so the cast is exact.
        let masked_tail = vl::masked_fill_tail_code(&plan, &regs, ARRAY_DATA_DISP8 as i8);
        // JIT round 24 B1: the iota registers' set-up, decided before any
        // byte is emitted.
        let iota_init: Option<Vec<u8>> = match vl::iota_regs(&plan, &inv_regs) {
            Some((run, step)) => match vl::iota_init_code(plan.kind, run, step) {
                Some(code) => Some(code),
                None => return self.vec_declined(&plan, "no registers for the induction lanes"),
            },
            None if plan.iota => {
                return self.vec_declined(&plan, "no registers for the induction lanes")
            }
            None => None,
        };
        // A scoped store's base is rebased onto its segment start below
        // (`SUB r, HEADER_SIZE` as a sign-extended byte, through the checked
        // `disp8_const`: a header past 127 fails the build).
        // A native one resolves its arena handle through the JIT-readable
        // table, as each scalar access does.
        let arena_table = scoped_arena_jit_table(&self.direct_helpers);
        if plan.scoped.is_some_and(|s| s.native) && arena_table.is_none() {
            return self.vec_declined(&plan, "a native scoped store with no arena table");
        }
        // A byte-lane STORE (VEC21-3) also needs the `boolean[]` kind byte, to
        // refuse an array whose `bastore` masks; decided before any byte is
        // emitted, like every refusal above.
        let byte_store = plan.base_kinds.get(plan.store_base) == Some(&MemKind::Byte)
            && plan.reduce.is_none()
            && plan.scan.is_none();
        let bool_store_tag: Option<u8> = if byte_store {
            match cratonvm_types::primitive_array_kind_tags_byte("[Z") {
                Some(tag) => Some(tag),
                None => return self.vec_declined(&plan, "no `boolean[]` kind tag"),
            }
        } else {
            None
        };
        // ── Guards: nothing Java-visible has changed until they all pass ──
        let mut bail: Vec<usize> = Vec::new();
        self.gp_load_value(RDX, plan.iv);
        self.buf.emit(&[0x48, 0x63, 0xD2]); // MOVSXD RDX, EDX
        const LEN_DISP: u8 = crate::x64::disp::disp8_const(ARRAY_LENGTH_OFFSET as i64) as u8;
        match plan.bound_length_of {
            Some(arr) => {
                // n = arr.length, read once: a null `arr` leaves for the
                // scalar header, whose own `arraylength` throws there.
                self.gp_load_value(R10, arr);
                self.buf.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
                bail.push(self.emit_jcc_rel32(0x84)); // JZ bail
                self.buf.emit(&[0x45, 0x8B, 0x52, LEN_DISP]); // MOV R10D, [R10 + len]
            }
            None => {
                self.gp_load_value(R10, plan.bound);
                self.buf.emit(&[0x4D, 0x63, 0xD2]); // MOVSXD R10, R10D
            }
        }
        let bypass = self.trivial_vec_header_exit(succ_block, &plan);
        let small_eligible = plan.reduce.is_none()
            && plan.scan.is_none()
            && !plan.iota
            && plan.scoped.is_none()
            && plan.widen.is_none()
            && plan.shift_counts.is_empty()
            && plan.bases.len() == 1
            && matches!(plan.value, vl::VExpr::Inv(0))
            && plan.invariants.len() == 1
            && matches!(
                plan.kind,
                vl::VecKind::Int | vl::VecKind::Long | vl::VecKind::Byte | vl::VecKind::Short
            );
        let mut empty_or_small_exit: Vec<usize> = Vec::new();
        self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        bail.push(self.emit_jcc_rel32(0x88)); // JS bail
        // Check `bound - j <= 0` up front (before per-array checks and vector
        // broadcasts): empty trips need no scalar header re-entry when the
        // header is pure, and non-small-fill sub-step loops bail before paying
        // broadcasts or `VZEROUPPER`.
        self.buf.emit(&[0x4D, 0x89, 0xD3]); // MOV R11, R10
        self.buf.emit(&[0x49, 0x29, 0xD3]); // SUB R11, RDX
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        match bypass {
            Some((_, true)) => {
                empty_or_small_exit.push(self.emit_jcc_rel32(0x8E)); // JLE exit
            }
            Some((_, false)) => {
                empty_or_small_exit.push(self.emit_jcc_rel32(0x84)); // JE exit
                bail.push(self.emit_jcc_rel32(0x88)); // JS bail
            }
            None if masked_tail.is_some() || small_eligible => {
                bail.push(self.emit_jcc_rel32(0x8E)); // JLE bail
            }
            None => {}
        }
        if masked_tail.is_none() && !small_eligible {
            self.buf.emit(&[0x49, 0x83, 0xFB]); // CMP R11, imm8
            // Cast: at most 64 lanes.
            self.buf.emit_byte(lanes as u8);
            bail.push(self.emit_jcc_rel32(0x8C)); // JL bail
        }
        // A scoped access is bounded by its ELEMENT width, which the lanes'
        // is not when they widen (`long` lanes over `int` elements, VEC27-1).
        let scoped_w = plan
            .base_kinds
            .get(plan.store_base)
            .map_or(plan.kind.elem_bytes(), |&k| crate::ir_vector_loop::mem_bytes(k));
        for (i, &b) in plan.bases.iter().enumerate() {
            let r = vl::BASE_REGS[i];
            // JIT round 24 B1: a native scoped store has no base object; its
            // element address comes from the segment's `min`.
            if let (Some(sc), true, Some(table)) =
                (plan.scoped.filter(|s| s.native), i == plan.store_base, arena_table)
            {
                debug_assert!(plan.bases.len() == 1 && r == 0, "B1: one base, in RAX");
                self.emit_vector_native_scoped_base(sc, scoped_w, table, &mut bail);
                continue;
            }
            self.gp_load_value(r, b);
            let rex_b = u8::from(r >= 8);
            // The bound is this array's own length (`i < a.length`): the bound
            // read above already left a null `a` for the scalar header, and
            // `n > a.length` cannot hold.
            let own_length = plan.bound_length_of == Some(b) && plan.scoped.is_none();
            if !own_length {
                // TEST r, r
                self.buf.emit(&[
                    0x48 | (rex_b << 2) | rex_b,
                    0x85,
                    0xC0 | ((r & 7) << 3) | (r & 7),
                ]);
                bail.push(self.emit_jcc_rel32(0x84)); // JZ bail
            }
            // JIT round 24 B1: a scoped store's lanes are bounded by its
            // SEGMENT, as each scalar access is (`0 <= k * w <= length - w`
            // for every `k` in `[j, n)`, with `j >= 0` tested above):
            //   MOV RCX, R10 ; IMUL RCX, RCX, w ; CMP RCX, R11(length) ; JG bail
            // and the base moves onto the segment's start, so the step's
            // `[r + j*w + HEADER_SIZE + u*32]` is `base + start + (j + u*W)*w`:
            //   ADD r, R11(start) ; SUB r, HEADER_SIZE
            // `r` is then a derived address no safepoint ever sees: the loop
            // leaves before its poll's pause, and the base itself stays live
            // for the scalar loop.
            if let (Some(sc), true) = (plan.scoped, i == plan.store_base) {
                debug_assert!(plan.bases.len() == 1 && r == 0, "B1: one base, in RAX");
                let w = scoped_w;
                if !self.emit_vector_scoped_length(sc, &mut bail) {
                    // Unreachable: the plan carried a length field the
                    // expansion resolved; without its offsets no lane runs.
                    bail.push(self.emit_jmp_rel32());
                }
                self.buf.emit(&[0x4C, 0x89, 0xD1]); // MOV RCX, R10
                self.buf.emit(&[0x48, 0x6B, 0xC9, w]); // IMUL RCX, RCX, imm8
                self.buf.emit(&[0x4C, 0x39, 0xD9]); // CMP RCX, R11
                bail.push(self.emit_jcc_rel32(0x8F)); // JG bail
                self.gp_load_value(R11, sc.start);
                self.buf.emit(&[0x4C, 0x01, 0xD8]); // ADD RAX, R11
                let hdr = crate::x64::disp::disp8_const(HEADER_SIZE as i64) as u8;
                self.buf.emit(&[0x48, 0x83, 0xE8, hdr]); // SUB RAX, imm8
                self.note_rcx_written();
                continue;
            }
            // CMP R10D, [r + len] -- n > length: some lane would be past it.
            if !own_length {
                self.buf
                    .emit(&[0x44 | rex_b, 0x3B, 0x40 | (2 << 3) | (r & 7), LEN_DISP]);
                bail.push(self.emit_jcc_rel32(0x8F)); // JG bail
            }
            // VEC21-3: a byte-lane store into a `boolean[]` owes JVMS 6.5's
            // `value & 1`, which a raw byte lane does not apply; such a call
            // leaves for the scalar loop, whose `bastore` masks.
            if let (Some(tag), true) = (bool_store_tag, i == plan.store_base) {
                // CMP BYTE [r + KIND_TAGS_BYTE_OFFSET], tag("[Z")
                if rex_b != 0 {
                    self.buf.emit_byte(0x41);
                }
                self.buf.emit(&[
                    0x80,
                    0x78 | (r & 7),
                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                    tag,
                ]);
                bail.push(self.emit_jcc_rel32(0x84)); // JE bail
            }
        }
        // VEC21-8: a sub-word scan compares lanes of the element width, so a
        // runtime `int` operand must be a value the element's own extension
        // reproduces; any other leaves for the scalar loop, which answers
        // every element by itself. (A constant was judged by the planner.)
        if let Some((bits, signed)) = plan.scan.as_ref().and_then(|s| s.narrow) {
            for &v in &plan.invariants {
                if matches!(
                    self.graph.nodes.get(v as usize).map(|n| &n.op),
                    Some(Op::Const(_))
                ) {
                    continue;
                }
                self.gp_load_value(R11, v);
                // MOVSX / MOVZX R9D, R11B / R11W
                let opc = match (bits, signed) {
                    (8, true) => 0xBE,
                    (8, false) => 0xB6,
                    (_, true) => 0xBF,
                    (_, false) => 0xB7,
                };
                self.buf.emit(&[0x45, 0x0F, opc, 0xCB]);
                self.buf.emit(&[0x45, 0x39, 0xD9]); // CMP R9D, R11D
                bail.push(self.emit_jcc_rel32(0x85)); // JNE bail
            }
        }
        // A guarded scalar intrinsic in the loop (the min/max reduction's
        // `Math.max` at a first-call site,
        // `ir::scalar_intrinsic_first_call_enabled`) resolves at its first
        // execution: when the pre-loop will take a step, that execution is
        // here. Emitted before any vector register is live (the guard's helper
        // keeps only the low 128 bits of the XMM file). `R10` = n, `RDX` = j.
        let guard_cells: Vec<usize> = self
            .graph
            .scalar_first_call_cells
            .iter()
            .filter(|&&(pc, op, _)| {
                self.graph.nodes.iter().enumerate().any(|(i, n)| {
                    n.op == Op::ScalarIntrinsic(op)
                        && n.bytecode_pc == Some(pc)
                        && self
                            .schedule
                            .node_to_block
                            .get(i)
                            .is_some_and(|b| plan.loop_blocks.contains(b))
                })
            })
            .map(|&(_, _, cell)| cell)
            .collect();
        if !guard_cells.is_empty() {
            self.buf.emit(&[0x4D, 0x89, 0xD3]); // MOV R11, R10
            self.buf.emit(&[0x49, 0x29, 0xD3]); // SUB R11, RDX
            self.buf.emit(&[0x49, 0x83, 0xFB]); // CMP R11, imm8
                                                // Cast: at most 32 lanes.
            self.buf.emit_byte(lanes as u8);
            let no_step = self.emit_jcc_rel32(0x8C); // JL .no_step
            for cell in guard_cells {
                crate::runtime_lowering::emit_first_call_guard(&mut self.buf, cell);
            }
            self.patch_rel32_to_here(no_step);
        }
        if small_eligible {
            self.buf.emit(&[0x4D, 0x89, 0xD3]); // MOV R11, R10
            self.buf.emit(&[0x49, 0x29, 0xD3]); // SUB R11, RDX
            self.buf.emit(&[0x49, 0x83, 0xFB, 0x04]); // CMP R11, 4
            let do_vector = self.emit_jcc_rel32(0x8D); // JGE do_vector
            self.emit_small_scalar_fill(plan.invariants[0], plan.mem_elem_bytes());
            self.buf.emit(&[0x4C, 0x89, 0xD2]); // MOV RDX, R10
            let iv_slot = self.slot_of(iv);
            self.store_abi_reg(RDX, iv_slot);
            if let Some(dst) = self.assigned_gpr(iv) {
                self.emit_mov_reg_reg64(dst, RDX);
            }
            if bypass.is_some() {
                empty_or_small_exit.push(self.emit_jmp_rel32());
            } else {
                bail.push(self.emit_jmp_rel32());
            }
            self.patch_rel32_to_here(do_vector);
            if masked_tail.is_none() {
                self.buf.emit(&[0x49, 0x83, 0xFB]); // CMP R11, imm8
                // Cast: at most 64 lanes.
                self.buf.emit_byte(lanes as u8);
                bail.push(self.emit_jcc_rel32(0x8C)); // JL bail
            }
        }
        // ── The invariants, broadcast once ──
        for (i, &v) in plan.invariants.iter().enumerate() {
            let dst = inv_regs[i];
            let konst = match self.graph.nodes.get(v as usize).map(|n| &n.op) {
                Some(Op::Const(c)) => Some(*c as u64),
                Some(Op::ConstF(bits)) => Some(*bits),
                _ => None,
            };
            match (konst, plan.kind) {
                (Some(bits), _) => {
                    self.emit_mov_reg_imm64(R11, bits);
                    self.buf.emit(&[0x66, 0x49, 0x0F, 0x6E, 0xC3]); // MOVQ XMM0, R11
                }
                (None, vl::VecKind::Double) => self.fp_load_value(XMM0, v, true),
                (None, vl::VecKind::Float) => self.fp_load_value(XMM0, v, false),
                // Int / Long, and a sub-word lane, which broadcasts the low
                // byte / word of the `int`.
                (None, _) => {
                    self.gp_load_value(R11, v);
                    self.buf.emit(&[0x66, 0x49, 0x0F, 0x6E, 0xC3]); // MOVQ XMM0, R11
                }
            }
            let mut b = Vec::new();
            if plan.uses_avx512() {
                vl::wide_broadcast(&mut b, plan.kind, dst, XMM0);
            } else {
                vl::emit_broadcast(&mut b, plan.kind, dst, XMM0);
            }
            self.buf.emit(&b);
        }
        // ── The masked tail: `1 <= n - j < lanes` elements in one store,
        // then `j = n` written back as the vector loop's own exit does ──
        let mut tail_done: Option<usize> = None;
        if let Some(code) = &masked_tail {
            self.buf.emit(&[0x4D, 0x89, 0xD3]); // MOV R11, R10
            self.buf.emit(&[0x49, 0x29, 0xD3]); // SUB R11, RDX
            self.buf.emit(&[0x49, 0x83, 0xFB]); // CMP R11, imm8
            // Cast: at most 64 lanes.
            self.buf.emit_byte(lanes as u8);
            let wide = self.emit_jcc_rel32(0x8D); // JGE wide
            self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
            let none_left = self.emit_jcc_rel32(0x8E); // JLE: the header exits
            self.buf.emit(code);
            self.buf.emit(&[0x4C, 0x89, 0xD2]); // MOV RDX, R10
            if bypass.is_some() {
                let iv_slot = self.slot_of(iv);
                self.store_abi_reg(RDX, iv_slot);
                if let Some(dst) = self.assigned_gpr(iv) {
                    self.emit_mov_reg_reg64(dst, RDX);
                }
                self.buf.emit(&[0xC5, 0xF8, 0x77]); // VZEROUPPER
                empty_or_small_exit.push(self.emit_jmp_rel32());
            } else {
                tail_done = Some(self.emit_jmp_rel32());
            }
            self.patch_rel32_to_here(none_left);
            // VZEROUPPER on the way to the header, as the vector exit does.
            self.buf.emit(&[0xC5, 0xF8, 0x77]);
            bail.push(self.emit_jmp_rel32());
            if let Some((exit_block, _)) = bypass {
                if !empty_or_small_exit.is_empty() {
                    for p in empty_or_small_exit.drain(..) {
                        self.patch_rel32_to_here(p);
                    }
                    BLOCK_JMPS_EMITTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.buf.emit_byte(0xE9); // JMP exit_block
                    let patch_pos = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    self.branch_patches.push((patch_pos, exit_block));
                }
            }
            self.patch_rel32_to_here(wide);
        }
        if let Some((exit_block, _)) = bypass {
            if !empty_or_small_exit.is_empty() {
                let over_stub = self.emit_jmp_rel32();
                for p in empty_or_small_exit {
                    self.patch_rel32_to_here(p);
                }
                BLOCK_JMPS_EMITTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.buf.emit_byte(0xE9); // JMP exit_block
                let patch_pos = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                self.branch_patches.push((patch_pos, exit_block));
                self.patch_rel32_to_here(over_stub);
            }
        }
        // ── VEC21-6: each loop-invariant shift count, masked, in its own
        // register (the low quadword is the count every lane shifts by) ──
        for (c, &k) in plan.shift_counts.iter().enumerate() {
            self.gp_load_value(R11, k);
            self.buf.emit(&vl::count_load_code(
                plan.kind,
                inv_regs[plan.invariants.len() + c],
            ));
        }
        // ── VEC21-2: the narrowing store's shuffle control ──
        if let Some(mask) = narrow_mask {
            self.emit_mov_reg_imm64(R11, mask);
            self.buf.emit(&vl::narrow_mask_load_code(
                inv_regs[plan.invariants.len() + plan.shift_counts.len()],
            ));
        }
        // ── JIT round 24 B1: `j + [0, W)` and `W`, before the polls take R11 ──
        if let Some(code) = &iota_init {
            self.buf.emit(code);
        }
        // A reduction's accumulator starts at zero lanes: after the
        // broadcasts, which stage through XMM0. A min/max accumulator
        // (VEC21-5) starts at its entry value in every lane instead (min and
        // max are idempotent), so the fold is the new value outright; an FP
        // one seeds its NaN-seen register from that value too.
        if let (Some(a), Some(acc), Some(op)) = (acc_vreg, acc_phi, red_op) {
            let mut z = Vec::new();
            if op.is_minmax() {
                match plan.kind {
                    vl::VecKind::Double => self.fp_load_value(XMM0, acc, true),
                    vl::VecKind::Float => self.fp_load_value(XMM0, acc, false),
                    _ => {
                        self.gp_load_value(R11, acc);
                        self.buf.emit(&[0x66, 0x49, 0x0F, 0x6E, 0xC3]); // MOVQ XMM0, R11
                    }
                }
                vl::emit_broadcast(&mut z, plan.kind, a, XMM0);
                if let Some(n) = nan_vreg {
                    z.extend(vl::minmax_nan_init_code(plan.kind, a, n));
                }
            } else if ordered_fp {
                self.fp_load_value(XMM0,acc,plan.kind==vl::VecKind::Double);
                z.extend(vl::scalar_vector_move(a,XMM0));
            } else {
                vl::emit_zero(&mut z, a);
            }
            self.buf.emit(&z);
        }
        let polls = crate::x64::jit_safepoint_polls_enabled() && self.safepoint_flag_addr != 0;
        if polls {
            // Interpreter round i6 wave 120, lane L2 (`CRATONVM_JIT_POLL_THREAD_WORD`,
            // default OFF; i24-L3 stage 3): the loop's polls test whatever R11
            // names, once chosen here -- this thread's poll word when its
            // mirror is set (a door of this VM published it), else the baked
            // flag. A set pause byte leaves the vector loop for the scalar
            // remainder, whose own poll parks, exactly as the flag did.
            //
            //   MOV R11, <seg>:[disp32] ; TEST R11, R11 ; JNZ .have
            //   MOV R11, imm64 <flag>   ; .have:
            let have = if crate::x64::jit_poll_thread_word_enabled() {
                // Cast: the probe answers a 32-bit segment displacement.
                let disp = crate::x64::loop_poll_word_tls_disp() as u32;
                self.buf
                    .emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
                self.buf.emit(&[0x4C, 0x8B, 0x1C, 0x25]); // MOV R11, <seg>:[disp32]
                self.buf.emit(&disp.to_le_bytes());
                self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
                Some(self.emit_jcc_rel32(0x85)) // JNZ .have
            } else {
                None
            };
            self.emit_mov_reg_imm64(R11, self.safepoint_flag_addr as u64);
            if let Some(have) = have {
                self.patch_rel32_to_here(have);
            }
            if owed_latch != 0 {
                debug_assert_eq!(plan.bases.len(), 1);
                self.emit_mov_reg_imm64(RCX, owed_latch as u64);
            }
        }
        let mut done: Vec<usize> = tail_done.into_iter().collect();
        // Either request leaves through the write-back before the scalar poll
        // can park. No derived address or vector-only accumulator is exposed.
        let emit_pause_checks = |this: &mut Self, done: &mut Vec<usize>| {
            if polls {
                this.buf.emit(&[0x41, 0xF6, 0x03, 0xFF]); // TEST byte [R11], 0xFF
                done.push(this.emit_jcc_rel32(0x85)); // JNZ done
                if owed_latch != 0 {
                    this.buf.emit(&[0xF6, 0x01, 0xFF]); // TEST byte [RCX], 0xFF
                    done.push(this.emit_jcc_rel32(0x85)); // JNZ done
                }
            }
        };
        let unroll = plan.unroll() as i32;
        // ── Main loop: UNROLL steps per trip while j + lanes*UNROLL <= n ──
        self.buf.emit(&[0x49, 0x81, 0xEA]); // SUB R10, imm32
        self.buf.emit(&(lanes * unroll).to_le_bytes());
        self.buf.emit(&[0x4C, 0x39, 0xD2]); // CMP RDX, R10
        let skip_main = self.emit_jcc_rel32(0x8F); // JG after_main
        let main_top = self.buf.pos();
        emit_pause_checks(self, &mut done);
        // VEC21-8: a scan step ends in `JNZ rel32` to its exit; (operand, k).
        let mut scan_exits: Vec<(usize, usize)> = Vec::new();
        let is_scan = plan.scan.is_some();
        for (k, s) in steps.iter().enumerate() {
            self.buf.emit(s);
            if is_scan {
                scan_exits.push((self.buf.pos() - 4, k));
            }
        }
        self.buf.emit(&[0x48, 0x81, 0xC2]); // ADD RDX, imm32
        self.buf.emit(&(lanes * unroll).to_le_bytes());
        self.buf.emit(&[0x4C, 0x39, 0xD2]); // CMP RDX, R10
        self.buf.emit(&[0x0F, 0x8E]); // JLE main_top
        let rel = main_top as i32 - (self.buf.pos() as i32 + 4);
        self.buf.emit(&rel.to_le_bytes());
        self.patch_rel32_to_here(skip_main);
        // ── One step per trip while j + lanes <= n ──
        self.buf.emit(&[0x49, 0x81, 0xC2]); // ADD R10, imm32
        self.buf.emit(&(lanes * (unroll - 1)).to_le_bytes());
        let one_top = self.buf.pos();
        self.buf.emit(&[0x4C, 0x39, 0xD2]); // CMP RDX, R10
        done.push(self.emit_jcc_rel32(0x8F)); // JG done
        emit_pause_checks(self, &mut done);
        self.buf.emit(&steps[0]);
        if is_scan {
            scan_exits.push((self.buf.pos() - 4, 0));
        }
        self.buf.emit(&[0x48, 0x81, 0xC2]); // ADD RDX, imm32
        self.buf.emit(&lanes.to_le_bytes());
        self.buf.emit_byte(0xE9); // JMP one_top
        let rel = one_top as i32 - (self.buf.pos() as i32 + 4);
        self.buf.emit(&rel.to_le_bytes());
        // ── VEC21-8: step k's exit, `j` moved onto the first hit lane ──
        if let Some(s) = plan.scan.as_ref() {
            for k in 0..plan.unroll() {
                let mut any = false;
                for &(p, _) in scan_exits.iter().filter(|&&(_, kk)| kk == k) {
                    self.patch_rel32_to_here(p);
                    any = true;
                }
                if any {
                    // Cast: k < UNROLL.
                    let code =
                        vl::scan_exit_code(plan.kind, s.mask_complemented(), k as i32 * lanes);
                    self.buf.emit(&code);
                    done.push(self.emit_jmp_rel32());
                }
            }
        }
        // ── done: j (in [entry, n], non-negative) back into the phi ──
        for p in done {
            self.patch_rel32_to_here(p);
        }
        // VEC21-5: an FP min/max that saw a NaN (an element, or the seed)
        // discards everything: nothing has been written back, so leaving for
        // the scalar header now runs the whole loop from its entry, and the
        // NaN comes back exactly as Java's sequential `Math.min/max` answers.
        if let Some(n) = nan_vreg {
            self.buf.emit(&vl::minmax_nan_test_code(n));
            let ok = self.emit_jcc_rel32(0x84); // JZ ok
            self.buf.emit(&[0xC5, 0xF8, 0x77]); // VZEROUPPER
            bail.push(self.emit_jmp_rel32()); // JMP bail
            self.patch_rel32_to_here(ok);
        }
        // A reduction: the lanes' wrapped sum (EAX / RAX) added to the
        // accumulator's entry value, back into its phi (VEC21-4). Every lane
        // holds the sum of the elements before `j` in its residue class, so
        // the total is the scalar sum of elements `entry..j` mod 2^32 / 2^64.
        // A min/max (VEC21-5) fold IS the new value (the lanes were seeded
        // with the entry value); an FP one is in XMM0, written after
        // VZEROUPPER below.
        if let (Some(acc), Some(code), Some(op)) = (acc_phi, fold.as_ref(), red_op) {
            self.buf.emit(code);
            if !fp_acc {
                if op.is_minmax() {
                    if plan.kind == vl::VecKind::Long {
                        self.buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
                    } else {
                        self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX
                        self.buf.emit(&[0x48, 0x63, 0xC9]); // MOVSXD RCX, ECX
                    }
                } else {
                    self.gp_load_value(RCX, acc);
                    if plan.kind == vl::VecKind::Long {
                        self.buf.emit(&[0x48, 0x01, 0xC1]); // ADD RCX, RAX
                    } else {
                        self.buf.emit(&[0x01, 0xC1]); // ADD ECX, EAX
                        self.buf.emit(&[0x48, 0x63, 0xC9]); // MOVSXD RCX, ECX
                    }
                }
                if acc_home_live {
                    let off = self.slot_of(acc);
                    self.store_abi_reg(RCX, off);
                }
                if let Some(r) = acc_reg {
                    // r22 wave 1 review point: residency register is never RAX, RCX, or RDX
                    debug_assert!(!matches!(r, RAX | RCX | RDX));
                    self.emit_mov_reg_reg64(r, RCX);
                }
            }
        }
        self.buf.emit(&[0xC5, 0xF8, 0x77]); // VZEROUPPER
        if let (true, Some(acc)) = (fp_acc, acc_phi) {
            let dbl = plan.kind == vl::VecKind::Double;
            if acc_home_live {
                let off = self.slot_of(acc);
                self.fp_store(off, XMM0, dbl);
            }
            if let Some(r) = acc_xreg {
                self.fp_reg_move(r, XMM0);
            }
        }
        if home_live {
            let off = self.slot_of(iv);
            self.store_abi_reg(RDX, off);
        }
        if let Some(r) = iv_reg {
            debug_assert!(!matches!(r, RAX | RCX | RDX));
            self.emit_mov_reg_reg64(r, RDX);
        }
        for p in bail {
            self.patch_rel32_to_here(p);
        }
        self.note_rcx_written();
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir-vec] {} bci={:?} emitted bits={} lanes={} ordered_fp={}",
                self.graph.dbg_label,
                plan.header_bci,
                if plan.uses_avx512() { 512 } else { 256 },
                lanes,
                ordered_fp
            );
        }
        self.vec_loops_emitted += 1;
    }

    /// JIT round 24 B1: `R11 = seg.length`, read from the segment itself (the
    /// access's receiver, `sc.recv`, which the loop keeps live): the
    /// expansion's own length read serves only the bounds guards, so once
    /// those leave the loop nothing keeps its home alive to the loop entry
    /// (a first build read a home the allocator had handed to `(n - 1) * w`,
    /// and every call left for the scalar loop). The read's own tests leave
    /// for the scalar loop too, never deopt:
    ///
    /// ```text
    ///   R9 = seg ; TEST R9, R9 ; JZ bail ; CMP DWORD [R9], class ; JNE bail
    ///   <layout epoch compare> ; JNE bail
    ///   TEST BYTE [R9 + flags], COMPACT ; JZ legacy
    ///   MOV R11, [R9 + compact] ; JMP done ; legacy: MOV R11, [R9 + legacy]
    /// ```
    ///
    /// Clobbers R9, R11, RCX (the epoch compare's fallback) and the flags;
    /// `false` (nothing emitted) when the field's offsets no longer resolve.
    pub(in crate::ir_lower) fn emit_vector_scoped_length(
        &mut self,
        sc: crate::ir_vector_loop::ScopedStore,
        bail: &mut Vec<usize>,
    ) -> bool {
        let (cid, op) = sc.length_field;
        let Some((compact_off, legacy_off)) = crate::ir::unbox_offsets(op, cid) else {
            return false;
        };
        self.gp_load_value(R9, sc.recv);
        self.buf.emit(&[0x4D, 0x85, 0xC9]); // TEST R9, R9
        bail.push(self.emit_jcc_rel32(0x84)); // JZ bail
        self.buf.emit(&[0x41, 0x81, 0x39]); // CMP DWORD [R9], imm32
        self.buf.emit(&cid.to_le_bytes());
        bail.push(self.emit_jcc_rel32(0x85)); // JNE bail
        if self.emit_layout_epoch_cmp() {
            bail.push(self.emit_jcc_rel32(0x85)); // JNE bail
        }
        self.buf.emit(&[0x41, 0xF6, 0x81]); // TEST BYTE [R9 + disp32], imm8
        self.buf
            .emit(&(cratonvm_types::GC_FLAGS_BYTE_OFFSET as i32).to_le_bytes());
        self.buf.emit(&[cratonvm_types::GC_FLAG_COMPACT]);
        let to_legacy = self.emit_jcc_rel32(0x84); // JZ legacy
        self.buf.emit(&[0x4D, 0x8B, 0x99]); // MOV R11, [R9 + disp32]
        self.buf.emit(&compact_off.to_le_bytes());
        let done = self.emit_jmp_rel32();
        self.patch_rel32_to_here(to_legacy);
        self.buf.emit(&[0x4D, 0x8B, 0x99]); // MOV R11, [R9 + disp32]
        self.buf.emit(&legacy_off.to_le_bytes());
        self.patch_rel32_to_here(done);
        self.note_rcx_written();
        true
    }

    /// JIT round 24 B1: the base register (RAX) of a NATIVE scoped store's
    /// vector pre-loop, entered with `RDX` = `j` (non-negative) and `R10` =
    /// `n`: the segment's `min` resolved ONCE for the whole range `[min, min +
    /// n * w)`, then less `HEADER_SIZE`, so the step's `[RAX + j*w + HEADER_SIZE
    /// + u*32]` is the element. Every failed test leaves for the scalar loop
    /// (`bail`), which resolves each access itself.
    ///
    /// ```text
    ///   R11 = session (pre-check) ; TEST R11, R11 ; JNZ bail     ; must read null
    ///   R11 = seg.length (`emit_vector_scoped_length`)
    ///   MOV RCX, R10 ; IMUL RCX, RCX, w ; CMP RCX, R11 ; JG bail
    ///   R11 = min ; TEST R11, R11 ; JLE bail                     ; the scalar arm's `offset > 0`
    ///   MOV RAX, 1 << 62 ; TEST R11, RAX ; JZ raw
    ///   R8 = table + ((R11 >> 30) & mask) * 32                   ; the handle's slot
    ///   RAX = R11 & !(2^30 - 1) ; CMP RAX, [R8] ; JNE bail       ; the block's key
    ///   ECX = R11D & (2^30 - 1)                                  ; offset in the block
    ///   MOV RAX, R10 ; IMUL RAX, RAX, w ; ADD RAX, RCX ; CMP RAX, [R8+8] ; JA bail
    ///   MOV RAX, [R8+16] ; ADD RAX, RCX                          ; the real address
    ///   R9 = R11 & !(2^30 - 1) ; CMP R9, [R8] ; JNE bail         ; still that block
    ///   JMP have
    /// raw:  MOV RAX, R11
    /// have: SUB RAX, HEADER_SIZE
    /// ```
    ///
    /// Why one resolution serves the whole loop: the two key compares make
    /// `(ptr, len)` the block's own at that instant (`unsafe_arena`'s
    /// argument, `native-builtins` `unsafe_natives_ext.rs`), and nothing the
    /// pre-loop runs can free a block -- no call, no synchronisation, and its
    /// poll leaves BEFORE a pause. A free on another thread with no
    /// happens-before edge races each scalar access exactly as it races this
    /// one (the race that module documents, HotSpot's `free(3)` too); one
    /// ordered before the loop's entry clears the key first and the second
    /// compare sends the call to the scalar loop. The session read null: a
    /// confined session owned by this thread and open, or one that never
    /// closes -- no close can happen under the pre-loop either.
    pub(in crate::ir_lower) fn emit_vector_native_scoped_base(
        &mut self,
        sc: crate::ir_vector_loop::ScopedStore,
        w: u8,
        table: u64,
        bail: &mut Vec<usize>,
    ) {
        if sc.session != crate::ir::NO_NODE {
            self.gp_load_value(R11, sc.session);
            self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
            bail.push(self.emit_jcc_rel32(0x85)); // JNZ bail: keep the session
        }
        if !self.emit_vector_scoped_length(sc, bail) {
            bail.push(self.emit_jmp_rel32());
        }
        self.buf.emit(&[0x4C, 0x89, 0xD1]); // MOV RCX, R10
        self.buf.emit(&[0x48, 0x6B, 0xC9, w]); // IMUL RCX, RCX, imm8
        self.buf.emit(&[0x4C, 0x39, 0xD9]); // CMP RCX, R11
        bail.push(self.emit_jcc_rel32(0x8F)); // JG bail
        self.gp_load_value(R11, sc.start);
        self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
        bail.push(self.emit_jcc_rel32(0x8E)); // JLE bail
        self.emit_mov_reg_imm64(RAX, 1u64 << 62);
        self.buf.emit(&[0x49, 0x85, 0xC3]); // TEST R11, RAX
        let raw = self.emit_jcc_rel32(0x84); // JZ raw
        const _: () = assert!(
            SCOPED_ARENA_SLOT_SHIFT == 30
                && SCOPED_ARENA_ENTRY_SHIFT == 5
                && SCOPED_ARENA_KEY_OFFSET == 0
                && SCOPED_ARENA_LEN_OFFSET == 8
                && SCOPED_ARENA_PTR_OFFSET == 16
        );
        let index_mask: u32 = (1u32 << SCOPED_ARENA_INDEX_BITS) - 1;
        let offset_mask: u32 = (1u32 << SCOPED_ARENA_SLOT_SHIFT) - 1;
        self.buf.emit(&[0x4D, 0x89, 0xD8]); // MOV R8, R11
        self.buf.emit(&[0x49, 0xC1, 0xE8, 0x1E]); // SHR R8, 30
        self.buf.emit(&[0x41, 0x81, 0xE0]); // AND R8D, imm32
        self.buf.emit(&index_mask.to_le_bytes());
        self.buf.emit(&[0x49, 0xC1, 0xE0, 0x05]); // SHL R8, 5
        self.emit_mov_reg_imm64(RAX, table);
        self.buf.emit(&[0x49, 0x01, 0xC0]); // ADD R8, RAX
        self.buf.emit(&[0x4C, 0x89, 0xD8]); // MOV RAX, R11
        self.buf.emit(&[0x48, 0xC1, 0xE8, 0x1E]); // SHR RAX, 30
        self.buf.emit(&[0x48, 0xC1, 0xE0, 0x1E]); // SHL RAX, 30
        self.buf.emit(&[0x49, 0x3B, 0x00]); // CMP RAX, [R8]  (key)
        bail.push(self.emit_jcc_rel32(0x85)); // JNE bail
        self.buf.emit(&[0x44, 0x89, 0xD9]); // MOV ECX, R11D
        self.buf.emit(&[0x81, 0xE1]); // AND ECX, imm32
        self.buf.emit(&offset_mask.to_le_bytes());
        self.buf.emit(&[0x4C, 0x89, 0xD0]); // MOV RAX, R10
        self.buf.emit(&[0x48, 0x6B, 0xC0, w]); // IMUL RAX, RAX, imm8
        self.buf.emit(&[0x48, 0x01, 0xC8]); // ADD RAX, RCX
        self.buf.emit(&[0x49, 0x3B, 0x40, 0x08]); // CMP RAX, [R8 + 8]  (len)
        bail.push(self.emit_jcc_rel32(0x87)); // JA bail
        self.buf.emit(&[0x49, 0x8B, 0x40, 0x10]); // MOV RAX, [R8 + 16]  (ptr)
        self.buf.emit(&[0x48, 0x01, 0xC8]); // ADD RAX, RCX
        self.buf.emit(&[0x4D, 0x89, 0xD9]); // MOV R9, R11
        self.buf.emit(&[0x49, 0xC1, 0xE9, 0x1E]); // SHR R9, 30
        self.buf.emit(&[0x49, 0xC1, 0xE1, 0x1E]); // SHL R9, 30
        self.buf.emit(&[0x4D, 0x3B, 0x08]); // CMP R9, [R8]  (key again)
        bail.push(self.emit_jcc_rel32(0x85)); // JNE bail
        let have = self.emit_jmp_rel32();
        self.patch_rel32_to_here(raw);
        self.buf.emit(&[0x4C, 0x89, 0xD8]); // MOV RAX, R11
        self.patch_rel32_to_here(have);
        let hdr = crate::x64::disp::disp8_const(HEADER_SIZE as i64) as u8;
        self.buf.emit(&[0x48, 0x83, 0xE8, hdr]); // SUB RAX, imm8
        self.note_rcx_written();
    }

    /// `CRATONVM_DBG_JITC`: the planner admitted `plan` but the emission
    /// declined it (the scalar loop runs every element).
    pub(in crate::ir_lower) fn vec_declined(&self, plan: &crate::ir_vector_loop::VecLoopPlan, why: &str) {
        if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir-vec] {} loop at bci {:?} declined at emission: {why}",
                self.graph.dbg_label, plan.header_bci
            );
        }
    }

    /// Write `id`'s result from `src`: ALWAYS to the home word, and
    /// additionally into its resident register.
    ///
    /// The write-through invariant lives here. The home store is emitted
    /// unconditionally and first, so the frame image is complete at every
    /// instruction boundary — which is what lets `emit_safepoint_map`,
    /// `build_deopt_points` and `emit_phi_copies` stay untouched.
    pub(in crate::ir_lower) fn gp_store_value(&mut self, id: NodeId, slot: i32, src: u8) {
        if self.poisons_int_def(id) {
            self.emit_poison_int_high(src);
        }
        self.store_abi_reg(src, slot);
        if let Some(dst) = self.assigned_gpr(id) {
            self.emit_mov_reg_reg64(dst, src);
            self.ls_spills += 1;
            self.mark_gp_reg_live(id);
        }
    }

    /// Publish a result already stored to its home word into its register — the
    /// memory → register transition, for definition sites whose result reaches
    /// the home word by a route this cache does not intercept.
    pub(in crate::ir_lower) fn publish_gp_from_slot(&mut self, id: NodeId, slot: i32) {
        if let Some(dst) = self.assigned_gpr(id) {
            self.load_reg_from_frame(dst, slot);
            self.ls_reloads += 1;
            self.mark_gp_reg_live(id);
        }
    }

    /// Make parameter node `id`'s home the word the prologue stored it to,
    /// `[rbp - param_offset]`, instead of a coloured slot holding a copy of it
    /// (round 11 wave 14, lane fib, proposal F13-2). `false` -- and nothing
    /// changed -- for an offset that cannot be a home.
    ///
    /// Sound because that word is written exactly once, by the prologue (or,
    /// on an OSR entry, by the stub's seed of this very node's home), and never
    /// again: this tier keeps locals in SSA values, and every other home is a
    /// coloured slot past `first_spill`. So the word holds the parameter at
    /// every point of the frame, which is also what `frame_value_for` already
    /// assumed for a parameter whose node had not been emitted yet. What goes
    /// is the copy: one store per activation, and the `RAX` round trip of the
    /// load that preceded it.
    pub(in crate::ir_lower) fn alias_param_home(&mut self, id: NodeId, param_offset: i32) -> bool {
        let Some(home) = u32::try_from(param_offset).ok().and_then(NonZeroU32::new) else {
            return false;
        };
        // Only a word `emit_prologue` stores: `local_offset(i)` for a
        // parameter `i < num_params`, all of them below `first_spill`.
        let stored_by_prologue = usize::try_from(param_offset)
            .is_ok_and(|off| off % 8 == 0 && off / 8 >= 1 && off / 8 <= self.num_params);
        if !stored_by_prologue || param_offset >= self.first_spill {
            return false;
        }
        let Some(cell) = self.node_slot.get_mut(id as usize) else {
            return false;
        };
        *cell = Some(home);
        // Defined from the prologue on; the relocation contract's "written by
        // the emitting code" holds for this word before the body runs.
        if let Some(defined) = self.defined_nodes.get_mut(id as usize) {
            *defined = true;
        }
        true
    }

    /// `MOV dst, src` — 64-bit register to register, for any pair including the
    /// extended registers the GP file is made of.
    pub(in crate::ir_lower) fn emit_mov_reg_reg64(&mut self, dst: u8, src: u8) {
        if dst == RCX {
            self.note_rcx_written();
        }
        self.emit_mov_reg_reg64_raw(dst, src);
    }

    /// [`Self::emit_mov_reg_reg64`] without the RCX bookkeeping, for the ONE
    /// site that writes RCX on purpose while a deferred carry is being
    /// INSTALLED: `store_rax` moving the carried value out of RAX. Every other
    /// caller wants the counter to move.
    pub(in crate::ir_lower) fn emit_mov_reg_reg64_raw(&mut self, dst: u8, src: u8) {
        if dst == src {
            return;
        }
        let rex = 0x48u8 | (((src >= 8) as u8) << 2) | ((dst >= 8) as u8);
        self.buf
            .emit(&[rex, 0x89, 0xC0 | ((src & 7) << 3) | (dst & 7)]);
    }

    /// `CMP a, b` — register to register, any pair, 64-bit when `wide`.
    ///
    /// `39 /r` is `CMP r/m, r`, so the FIRST operand goes in the ModRM `r/m`
    /// field and the second in `reg` — the opposite nesting from the mnemonic,
    /// and the reason this is a named helper rather than three inline literals.
    /// The flags it sets are `a - b`, which is the order `CmpCond::x64_cc`
    /// expects.
    ///
    /// The 32-bit form emits REX only when it has to name an extended register,
    /// so a comparison of two low registers stays two bytes exactly as
    /// `CMP EAX, ECX` did.
    pub(in crate::ir_lower) fn emit_cmp_reg_reg(&mut self, a: u8, b: u8, wide: bool) {
        let (bytes, len) = cmp_reg_reg_bytes(a, b, wide);
        self.buf.emit(&bytes[..len]);
    }

    /// `CMP a, [RBP - offset]` — 64-bit when `wide`.
    ///
    /// `3B /r` is `CMP r, r/m`, the mirror of the `39 /r` above: here the FIRST
    /// operand is the `reg` field and the second comes from memory, which is
    /// the direction needed to compare a resident value against one still in
    /// its frame slot. Flags are `a - [slot]`, the same order
    /// `CmpCond::x64_cc` expects.
    ///
    /// The 32-bit form reads four bytes where the `MOV` it replaces read eight.
    /// That is the same comparison: an `int` slot's value is its low word (the
    /// high half is either extension, whichever its producer left, and never
    /// read), and the `CMP EAX, ECX` this replaces only ever looked at those
    /// four bytes either.
    pub(in crate::ir_lower) fn emit_cmp_reg_frame(&mut self, a: u8, offset: i32, wide: bool) {
        let rex = if wide { 0x48u8 } else { 0x40u8 } | (((a >= 8) as u8) << 2);
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit_byte(0x3B);
        self.emit_rbp_modrm_disp(a, offset);
    }

    /// `CMP a, imm` — register against a folded constant, 64-bit when `wide`.
    pub(in crate::ir_lower) fn emit_cmp_reg_imm(&mut self, a: u8, imm: i32, wide: bool) {
        let (bytes, len) = cmp_reg_imm_bytes(a, imm, wide);
        self.buf.emit(&bytes[..len]);
    }

    /// `CMP [RBP - offset], imm` — a value still in its slot against a folded
    /// constant, 64-bit when `wide`. Three instructions become one, and the
    /// one names no register at all.
    pub(in crate::ir_lower) fn emit_cmp_frame_imm(&mut self, offset: i32, imm: i32, wide: bool) {
        let (bytes, len) = cmp_frame_imm_bytes(offset, imm, wide);
        self.buf.emit(&bytes[..len]);
    }

    /// Which in-place form a fused compare of `a` against `b` can take, if any.
    ///
    /// Ordered by how much each saves, and the immediate forms come first
    /// because they need the least: a constant folds into the instruction, so
    /// only the FIRST operand needs a place to be read from, and a first
    /// operand that lost its register is served as well as one that kept it.
    ///
    /// `alu_imm32` is the gate on the constant rather than a local
    /// `Op::Const` match, deliberately. It declines a constant too wide for
    /// `i32` — which every immediate form here sign-extends, so such a
    /// constant has no immediate encoding at all — and it is OFF under a MIR
    /// mode, where a tiled node is emitted by the selector and a fold here
    /// would make the byte-equality lane compare two different programs.
    ///
    /// `slot_of_checked` rather than `slot_of`: a dropped home declines the
    /// frame forms instead of latching a bailout on a path that has a
    /// perfectly good fallback.
    pub(in crate::ir_lower) fn pick_cmp_form(&self, a: NodeId, b: NodeId) -> Option<CmpForm> {
        if let Some(imm) = self.alu_imm32(b) {
            return match self.resident_gpr(a) {
                Some(ra) => Some(CmpForm::RegImm(ra, imm)),
                None => self
                    .slot_of_checked(a)
                    .ok()
                    .map(|off| CmpForm::FrameImm(off, imm)),
            };
        }
        // Both remaining forms need the first operand in a register: there is
        // no `CMP mem, mem`, and loading `a` to compare it against a slot
        // would spend the instruction this exists to remove.
        let ra = self.resident_gpr(a)?;
        match self.resident_gpr(b) {
            Some(rb) => Some(CmpForm::RegReg(ra, rb)),
            None => self
                .slot_of_checked(b)
                .ok()
                .map(|off| CmpForm::RegFrame(ra, off)),
        }
    }

    /// Set the flags for `a <cc> b` -- the compare of a FUSED `Op::Cmp`, which
    /// materialises no boolean -- in the cheapest form the operands' locations
    /// allow. Shared by the fused `If` (`lower_terminator`) and, since round 11,
    /// the fused `Op::Guard`; the caller emits the `Jcc`.
    ///
    /// 64-bit for `Ref` and `Long` operands; see the unfused `Op::Cmp` arm.
    ///
    /// Both already in registers: compare them there. A fused compare is the
    /// one site that may do this without owing anything else -- it defines no
    /// value, writes no home and publishes no register, so the only thing that
    /// outlives it is the flags, and those are the same either way. Nothing
    /// downstream may assume RAX holds `a`.
    ///
    /// Four in-place forms -- see `pick_cmp_form` for which one wins and why.
    /// Neither operand may be a value some carry is holding: that has to be
    /// read through `gp_load_value` or the carry strands. `b` in a frame slot is
    /// the common case, not the fallback: `peak_live` routinely exceeds the
    /// register file, and a loop bound is exactly the long-lived value that
    /// loses. `b` a CONSTANT is commoner still -- `i < 100` is the shape of most
    /// Java loops.
    pub(in crate::ir_lower) fn emit_fused_int_compare(&mut self, a: NodeId, b: NodeId) {
        let ref_cmp = matches!(self.graph.nodes[a as usize].ty, IrType::Ref | IrType::Long)
            || matches!(self.graph.nodes[b as usize].ty, IrType::Ref | IrType::Long);
        let form = if ir_cmp_in_place_enabled() && !self.carry_names(a) && !self.carry_names(b) {
            self.pick_cmp_form(a, b)
        } else {
            None
        };
        match form {
            Some(CmpForm::RegReg(ra, rb)) => {
                self.emit_cmp_reg_reg(ra, rb, ref_cmp);
                self.cmp_in_place += 1;
            }
            Some(CmpForm::RegFrame(ra, off)) => {
                self.emit_cmp_reg_frame(ra, off, ref_cmp);
                self.cmp_in_place_frame += 1;
            }
            Some(CmpForm::RegImm(ra, imm)) => {
                self.emit_cmp_reg_imm(ra, imm, ref_cmp);
                self.cmp_imm += 1;
            }
            Some(CmpForm::FrameImm(off, imm)) => {
                self.emit_cmp_frame_imm(off, imm, ref_cmp);
                self.cmp_imm_frame += 1;
            }
            None => {
                // JIT rx27 wave 8: the first operand through RAX (a carry
                // arrives there), the second in place when it can be.
                self.gp_load_value(RAX, a);
                if self.emit_cmp_rax_with(b, ref_cmp) {
                    return;
                }
                self.gp_load_value(RCX, b);
                if ref_cmp {
                    self.buf.emit(&[0x48, 0x39, 0xC8]); // CMP RAX, RCX
                } else {
                    self.buf.emit(&[0x39, 0xC8]); // CMP EAX, ECX
                }
            }
        }
    }

    /// Emit a selected materialized comparison using the locations assigned
    /// to its inputs. Refuse before emission if the tile is not supported.
    pub(in crate::ir_lower) fn emit_selected_materialized_cmp(&mut self, id: NodeId, slot: i32) -> bool {
        use crate::x64::isel::{MInst, Ty};
        if !self.reg_alloc_on || self.mir_mode != MirMode::Off {
            return false;
        }
        let Some((cmp, cc)) = self.mir.as_ref().and_then(|plan| plan.materialized_cmp(id)) else {
            return false;
        };
        let input_ty = |v: NodeId| self.graph.nodes.get(v as usize).map(|n| n.ty);
        let (ty, a, b) = match cmp {
            MInst::CmpRR { ty, lhs, rhs } => (ty, lhs, Some(rhs)),
            MInst::CmpRI { ty, lhs, imm, .. } if i32::try_from(imm).is_ok() => (ty, lhs, None),
            MInst::TestRR { ty, reg } => (ty, reg, None),
            _ => return false,
        };
        let wide = match ty { Ty::I32 => false, Ty::I64 => true, _ => return false };
        let supported = |v| matches!(input_ty(v), Some(IrType::Int | IrType::Ref | IrType::Long));
        let node_wide = |v| matches!(input_ty(v), Some(IrType::Ref | IrType::Long));
        if !supported(a) || b.is_some_and(|v| !supported(v))
            || (node_wide(a) || b.is_some_and(node_wide)) != wide
            || self.graph.nodes.get(id as usize).is_none_or(|n| {
                !matches!(n.op, Op::Cmp(_)) || n.ty != IrType::Int
                    || n.inputs.iter().any(|&v| v != a && Some(v) != b && self.carry_names(v))
            })
        {
            return false;
        }
        match cmp {
            MInst::CmpRR { lhs, rhs, .. } => self.emit_fused_int_compare(lhs, rhs),
            MInst::CmpRI { lhs, imm, .. } => {
                // A carry has a single reader and must go through its checked
                // load protocol, even if an old register assignment exists.
                let resident = (!self.carry_names(lhs)
                    && !self.tail_carry.is_some_and(|(producer, _, _)| producer == lhs))
                    .then(|| self.resident_gpr(lhs)).flatten();
                if let Some(reg) = resident {
                    self.gp_reg_reads += 1;
                    self.emit_cmp_reg_imm(reg, imm as i32, wide);
                } else {
                    self.gp_load_value(RAX, lhs);
                    self.emit_cmp_reg_imm(RAX, imm as i32, wide);
                }
            }
            MInst::TestRR { reg, .. } => {
                let resident = (!self.carry_names(reg)
                    && !self.tail_carry.is_some_and(|(producer, _, _)| producer == reg))
                    .then(|| self.resident_gpr(reg)).flatten();
                if let Some(src) = resident {
                    use crate::x64::isel::{Op as MachineOp, Operand, Req};
                    self.gp_reg_reads += 1;
                    self.emit_field_instruction(Req::new(
                        MachineOp::Test, ty, Operand::Gpr(src), Operand::Gpr(src),
                    ));
                } else {
                    self.gp_load_value(RAX, reg);
                    if wide { self.buf.emit(&[0x48, 0x85, 0xc0]); }
                    else { self.buf.emit(&[0x85, 0xc0]); }
                }
            }
            _ => return false,
        }
        if let Some(dst) = self.integer_result_gpr(IrType::Int, slot) {
            // SETcc writes a byte; MOVZX defines the whole Java int. A REX
            // prefix is required for SPL/BPL/SIL/DIL as well as R8B–R15B.
            let low = dst & 7;
            if dst >= 4 {
                self.buf.emit_byte(0x40 | u8::from(dst >= 8));
            }
            self.buf.emit(&[0x0f, cc.x64_cc() + 0x10, 0xc0 | low]);
            if dst >= 4 {
                self.buf.emit_byte(0x40 | (u8::from(dst >= 8) << 2) | u8::from(dst >= 8));
            }
            self.buf.emit(&[0x0f, 0xb6, 0xc0 | (low << 3) | low]);
            self.publish_integer_result(dst, slot);
        } else {
            // SETcc and MOVZX overwrite the accumulator even when the compare
            // read its operands in place and never loaded RAX.
            self.rax_home_mirror = None;
            self.rax_reg_mirror = None;
            self.buf.emit(&[0x0f, cc.x64_cc() + 0x10, 0xc0]);
            self.buf.emit(&[0x0f, 0xb6, 0xc0]);
            self.store_rax(slot);
        }
        self.mir_tiles += 1;
        true
    }

    /// `x + k` (or `x - k`) as a `LEA`, when `k` is a constant and `x` is
    /// resident. Answers WHERE it left the result, so the caller knows how
    /// much of its accumulator sequence still has to run.
    ///
    /// What it replaces, from `CmpImm.wide` with `i` in RBX:
    ///
    /// ```asm
    /// mov rax,rbx        ; i -> RAX
    /// add eax,1
    /// ```
    ///
    /// Two instructions to add one to a value that was already in a register.
    /// `LEA` is the only three-operand integer instruction on this machine, so
    /// it is the only way to read a source and write a destination that is not
    /// the source without routing through the accumulator — and `x + 1` in a
    /// counted loop is exactly that shape.
    ///
    /// **Two forms, and the weaker one is the common case.** Which applies
    /// turns on whether the result got a register of its own:
    ///
    /// * it did — [`AddForm::Done`], `lea r14d,[rbx+1]` writes that register
    ///   directly and the arm owes nothing further;
    /// * it did not — [`AddForm::InRax`], `lea eax,[rbx+1]` writes the
    ///   accumulator and [`Self::store_rax`] finishes exactly as it would
    ///   have. One instruction out of two, and nothing else about the arm
    ///   changes.
    ///
    /// The second form is the one a loop-carried increment actually takes, and
    /// it is why this is not written as a single all-or-nothing fold. Measured
    /// on `CmpImm.wide`: `def_publishes=0` against `phi copies:
    /// reg_publishes=10` — the loop-carried values are published by the phi
    /// copies on the back edge, so `i + 1` itself writes a home word and is
    /// given no register. A first version that required one engaged **nowhere**
    /// on that probe.
    ///
    /// Unlike the fused compare this DEFINES a value, so the direct form owes
    /// everything a definition owes:
    ///
    /// * neither operand may be a value a carry is holding, which has to be
    ///   read through `gp_load_value` or the carry strands — and when `x` IS
    ///   carried in RAX the accumulator path is already one instruction, so
    ///   declining costs nothing;
    /// * the result must not itself be PLANNED as a carry: `store_rax` hands a
    ///   carried value to its consumer out of RAX, and the direct form never
    ///   puts it there. The accumulator form needs no such guard — it falls
    ///   into `store_rax` with RAX loaded, which is the contract that path
    ///   already has;
    /// * and when the home word survives (`home_dropped` is false — a deopt
    ///   frame names it, say) it is still written, from the destination
    ///   register, which by then holds exactly what RAX would have.
    ///
    /// The 32-bit form zero-extends into the destination, which is what `ADD
    /// EAX, imm` did and what the 64-bit home store then writes, so the frame
    /// image is byte-identical either way.
    pub(in crate::ir_lower) fn emit_add_lea(&mut self, id: NodeId, slot: i32, sub: bool, wide: bool) -> AddForm {
        if !ir_add_lea_enabled() {
            return AddForm::No;
        }
        let node = &self.graph.nodes[id as usize];
        let (x, k) = (node.inputs[0], node.inputs[1]);
        // Only the SECOND operand is tried as the constant. `Op::Add` is
        // commutative and nothing canonicalises it, so `1 + i` is left to the
        // accumulator path — a missed fold, never a wrong one, and javac does
        // not emit that order for a counted loop.
        let Some(imm) = self.alu_imm32(k) else {
            // Round 9 wave 10 (irl10): `x + y` with both operands in registers
            // has a `LEA` form too. `x - y` has none.
            return if sub {
                AddForm::No
            } else {
                self.emit_add_lea_reg_reg(id, slot, x, k, wide)
            };
        };
        // `x - k` is `x + (-k)`, except at `i32::MIN`, whose negation is not an
        // `i32` at all. One constant in the language, and it declines here
        // rather than wrapping into a silent `+ MIN`.
        let Some(disp) = (if sub { imm.checked_neg() } else { Some(imm) }) else {
            return AddForm::No;
        };
        if self.carry_names(x) || self.carry_names(k) {
            return AddForm::No;
        }
        let Some(base) = self.resident_gpr(x) else {
            return AddForm::No;
        };
        // The direct form, when the result has a register to be written into
        // and nothing has claimed RAX as the route to its consumer.
        if let Some(dst) = self.assigned_gpr(id) {
            if self.carry_at_store(slot).is_none() {
                let (bytes, len) = lea_reg_base_disp_bytes(dst, base, disp, wide);
                self.buf.emit(&bytes[..len]);
                if self.poisons_int_def(id) {
                    self.emit_poison_int_high(dst);
                }
                self.mark_gp_reg_live(id);
                self.reg_publishes_at_def += 1;
                self.cur_def_published = true;
                if self.home_dropped.get(id as usize).copied().unwrap_or(false) {
                    self.homes_dropped_at_def += 1;
                    self.home_stores_dropped += 1;
                } else {
                    self.store_abi_reg(dst, slot);
                }
                self.add_lea += 1;
                return AddForm::Done;
            }
        }
        let (bytes, len) = lea_reg_base_disp_bytes(RAX, base, disp, wide);
        self.buf.emit(&bytes[..len]);
        self.add_lea_acc += 1;
        AddForm::InRax
    }

    /// `x + y` as `LEA dst, [x + y]` when both operands are in registers --
    /// the register-register sibling of [`Self::emit_add_lea`]'s `x + k`, with
    /// the same two outcomes ([`AddForm::Done`] into the result's own register,
    /// [`AddForm::InRax`] into the accumulator for the caller's `store_rax`)
    /// and the same obligations for the direct one.
    ///
    /// Round 9 wave 10 (irl10),
    /// `perf-long-counted-loops-lose-the-optimizing-osr-body`: the
    /// accumulator `s += t` was `mov rax,r14; add rax,rcx; mov r12,rax` per
    /// iteration -- three instructions for one add of two register values.
    ///
    /// The second operand may be the value an RCX CARRY holds for exactly this
    /// node (`live_carry` naming RCX, or `deferred_rcx`, with this node as the
    /// planned consumer): it is then read out of RCX through `gp_load_value`,
    /// which retires the carry and emits nothing, and used as the `LEA` index.
    /// A value carried in RAX, or carried for a different consumer, declines,
    /// as does a first operand that is carried at all -- the accumulator path
    /// then reads it, and is already one instruction shorter for an RAX carry.
    ///
    /// Every check precedes the first byte, so a decline emits NOTHING and the
    /// caller's accumulator sequence runs exactly as before -- which matters
    /// for an RAX carry into this node, whose `gp_load_value` proves that
    /// nothing was emitted since its producer.
    ///
    /// Writes only the destination (the result's register, never RAX/RCX,
    /// which the GP file does not name) or RAX; RCX is only READ.
    pub(in crate::ir_lower) fn emit_add_lea_reg_reg(
        &mut self,
        id: NodeId,
        slot: i32,
        x: NodeId,
        y: NodeId,
        wide: bool,
    ) -> AddForm {
        if self.carry_names(x) {
            return AddForm::No;
        }
        let this_consumer = self.cur_def == Some(id);
        let y_in_rcx = this_consumer
            && (matches!(self.live_carry, Some((p, c, r, _)) if p == y && c == id && r == RCX)
                || matches!(self.deferred_rcx, Some((p, c, _)) if p == y && c == id));
        if !y_in_rcx && self.carry_names(y) {
            return AddForm::No;
        }
        let Some(base) = self.resident_gpr(x) else {
            return AddForm::No;
        };
        let index = if y_in_rcx {
            RCX
        } else {
            match self.resident_gpr(y) {
                Some(r) => r,
                None => return AddForm::No,
            }
        };
        if y_in_rcx {
            // Retires the carry: the value is already in RCX, so this emits
            // nothing. `y_in_rcx` asked exactly the three things
            // `gp_load_value`'s carry arms check (producer, planned consumer
            // is the node being lowered, register), so it takes the carry;
            // were it ever to refuse instead, it latches a bailout and this
            // artifact is discarded whatever the `LEA` below reads.
            self.gp_load_value(RCX, y);
        }
        // The direct form, when the result has a register to be written into
        // and nothing has claimed RAX as the route to its consumer.
        if let Some(dst) = self.assigned_gpr(id) {
            if self.carry_at_store(slot).is_none() {
                let (bytes, len) = lea_reg_base_index_bytes(dst, base, index, wide);
                self.buf.emit(&bytes[..len]);
                if self.poisons_int_def(id) {
                    self.emit_poison_int_high(dst);
                }
                self.mark_gp_reg_live(id);
                self.reg_publishes_at_def += 1;
                self.cur_def_published = true;
                if self.home_dropped.get(id as usize).copied().unwrap_or(false) {
                    self.homes_dropped_at_def += 1;
                    self.home_stores_dropped += 1;
                } else {
                    self.store_abi_reg(dst, slot);
                }
                self.add_lea += 1;
                return AddForm::Done;
            }
        }
        let (bytes, len) = lea_reg_base_index_bytes(RAX, base, index, wide);
        self.buf.emit(&bytes[..len]);
        self.add_lea_acc += 1;
        AddForm::InRax
    }

    /// Is `id` a value some carry is holding in RAX or RCX right now?
    ///
    /// A carried value must be read through `gp_load_value`, which is what
    /// takes it out of the slot; a site that reads it any other way would leave
    /// the carry stranded and refuse the compile. `plan_carries` already
    /// declines to carry anything the residency file assigned a register, so a
    /// resident value is never carried — this is the belt to that braces,
    /// because the failure is silent at the point it is made.
    pub(in crate::ir_lower) fn carry_names(&self, id: NodeId) -> bool {
        matches!(self.live_carry, Some((prod, _, _, _)) if prod == id)
            || matches!(self.deferred_rcx, Some((prod, _, _)) if prod == id)
    }

    /// Latch a structured bailout raised from an infallible legacy accessor.
    ///
    /// The first one wins (it is the cause; later ones are consequences of
    /// continuing to emit into an artifact that is already doomed).
    /// `lower_inner` takes it and reports it instead of returning a bare
    /// `None`. Interior mutability because both `&self` (`slot_of`) and
    /// `&mut self` (`alloc_slot`) accessors latch.
    pub(in crate::ir_lower) fn latch_bailout(&self, bailout: Bailout) {
        // JIT round 25 item 1a (wave 2): name the value, so the retry can
        // exclude it alone (`ElideRetryScope`).
        if let BailoutReason::UnallocatedValue { node } = &bailout.reason {
            self.note_elide_refusal(*node);
            if crate::x64::dbg_jitc_enabled() {
                eprintln!("[cratonvm-jitc] ir-elide-refused: {:?}", bailout);
            }
        }
        let mut slot = self.latched_bailout.borrow_mut();
        if slot.is_none() {
            *slot = Some(bailout);
        }
    }

    /// JIT round 25 item 1a (wave 2): record `node` as refused when one of this
    /// round's elisions planned it -- a tail carry, a site-named resident home
    /// or an unnamed compare fusion. Anything else is not this switch's.
    pub(in crate::ir_lower) fn note_elide_refusal(&self, node: NodeId) {
        let i = node as usize;
        let mine = self.tail_carry_of.get(i).copied().flatten().is_some()
            || self.site_named.get(i).copied().unwrap_or(false)
            || self.fused_cmp_unnamed.get(i).copied().unwrap_or(false)
            // JIT rx27 wave 6b: an `And` folded into its `If`'s `TEST`.
            || self.fused_test_and.get(i).copied().unwrap_or(false)
            // JIT round 25 item 2, wave 4: a lazy reference home.
            || self.lazy_ref_home.get(i).copied().unwrap_or(false)
            // JIT rx27 wave 9: a read skipped behind its publish.
            || self.publish_reload_elided.get(i).copied().unwrap_or(false)
            // Keep constants excluded on retry instead of falling back to `keep_deopt_image`.
            || self.is_dropped_const(node);
        if mine {
            note_elide_refused_value(node);
        }
    }

    /// Take the latched bailout, if any.
    pub(in crate::ir_lower) fn take_latched_bailout(&self) -> Option<Bailout> {
        self.latched_bailout.borrow_mut().take()
    }

    /// A frame offset that is safe to *emit* against but is not a real
    /// location: the first spill slot. Returned only by the legacy infallible
    /// accessors, and only after a bailout has been latched — which makes
    /// `lower_inner` discard the whole artifact before it can be executed.
    ///
    /// It exists so a doomed compile finishes walking the graph without
    /// panicking and without addressing memory outside the frame. The one
    /// value it must never be is `0`: that is `[rbp - 0]`, the saved caller
    /// frame pointer, which is exactly the read this whole change exists to
    /// make unrepresentable.
    pub(in crate::ir_lower) fn poison_slot(&self) -> i32 {
        debug_assert!(self.first_spill > 0);
        self.first_spill
    }

    /// Allocate a frame slot for a node result, or refuse the compile.
    ///
    /// real-frame-deopt (#6): the deopt stub calls `ir_deopt_entry` while the
    /// frame is live, with `rsp = rbp - frame_size`. The Win64 ABI requires 32
    /// bytes of caller shadow space at `[rsp, rsp+32)` — i.e. frame offsets
    /// `(frame_size-32 .. frame_size]`. A spill slot at offset `o` occupies
    /// `[rbp-o, rbp-o+8)`; to keep it clear of the shadow region we cap
    /// `o <= frame_size - DEOPT_SHADOW_SPACE`. `frame_size` already budgets the
    /// 32-byte shadow (plus a 16-byte stack-arg reserve), so this never rejects
    /// a method the old `o < frame_size` bound accepted.
    ///
    /// Exceeding that cap used to be an `assert!`, i.e. a panic on the compiler
    /// thread — which in a release VM is `fatal runtime error: failed to
    /// initiate panic` → SIGABRT of the whole process, for what is only a
    /// *compiler* resource limit. It is now a `FrameTooLarge` bailout: the
    /// method loses its optimized body and runs in a lower tier, which is
    /// always semantically valid.
    ///
    /// The offset is no longer a bump cursor: it is `first_spill + colour * 8`
    /// for the colour [`plan_slots`] assigned this value, so two values whose
    /// live ranges do not overlap land on the same word. Calling this twice for
    /// one node is therefore idempotent rather than wasteful. A node the plan
    /// does not cover is an internal inconsistency (the plan enumerates exactly
    /// the nodes `prealloc_phi_slots` and `lower_data_node` allocate for), and
    /// refuses the compile rather than inventing an offset.
    pub(in crate::ir_lower) fn alloc_slot_checked(&mut self, id: NodeId) -> CompileResult<i32> {
        let offset = self.planned_slot_off(id)?;
        // `planned_slot_off` already proved the offset non-zero and inside the
        // frame; re-deriving the `NonZeroU32` here keeps the "no zero offsets"
        // invariant enforced at the write, not merely upstream of it.
        let located = u32::try_from(offset)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| {
                Bailout::new(BailoutReason::Internal(
                    "ir_lower: spill offset 0 would encode [rbp - 0] (saved caller RBP)",
                ))
            })?;
        let cell = self.node_slot.get_mut(id as usize).ok_or_else(|| {
            Bailout::new(BailoutReason::Internal("ir_lower: node id out of range"))
        })?;
        *cell = Some(located);
        // Watermark, not a cursor: the highest word any emitted node's slot has
        // reached. Every safepoint's `live_frame_hi` is read off this, and with
        // reuse it stops growing once the peak live set is reached.
        self.spill_high_water = self.spill_high_water.max(offset.saturating_add(8));
        // Relocation contract: a slot becomes publishable the moment the
        // emitting code writes it. `alloc_slot` is called at the point of
        // emission for every node EXCEPT phis, whose slots are reserved up
        // front by `prealloc_phi_slots` and written later by edge copies —
        // those are marked separately, after the prologue zeroes them.
        if !matches!(self.graph.nodes[id as usize].op, Op::Phi) {
            self.defined_nodes[id as usize] = true;
        }
        Ok(offset)
    }

    /// The frame offset [`Self::alloc_slot_checked`] *will* assign `id`, without
    /// assigning it.
    ///
    /// Split out so the level-2 tile encoder can encode a store to a
    /// destination slot before the destination has been allocated, and get the
    /// same answer the allocating path would — by calling the same code, not by
    /// repeating the arithmetic. Every refusal below is the allocating path's
    /// refusal, unchanged; only the three mutations live in the caller.
    pub(in crate::ir_lower) fn planned_slot_off(&self, id: NodeId) -> CompileResult<i32> {
        let color = match self
            .slot_plan
            .node_color
            .get(id as usize)
            .copied()
            .flatten()
        {
            Some(color) => color,
            None => {
                return Err(Bailout::with_context(
                    BailoutReason::Internal(
                        "ir_lower: the slot plan has no colour for a value being lowered",
                    ),
                    format!(
                        "n{id} reached alloc_slot with no planned frame slot (op {:?})",
                        self.graph.nodes.get(id as usize).map(|n| &n.op),
                    ),
                ))
            }
        };
        let offset = i32::try_from(u64::from(color).saturating_mul(8))
            .ok()
            .and_then(|delta| self.first_spill.checked_add(delta))
            .ok_or_else(|| {
                Bailout::new(BailoutReason::Internal(
                    "ir_lower: coloured spill offset overflowed the frame arithmetic",
                ))
            })?;
        // `spill_cap_off` excludes the 32-byte shadow space AND (Gap B) the
        // Java-arg staging region, so a spill never overlaps either. For a
        // no-call method it equals `frame_size - DEOPT_SHADOW_SPACE` — the
        // historical bound, unchanged.
        if offset > self.spill_cap_off {
            return Err(Bailout::with_context(
                BailoutReason::FrameTooLarge {
                    bytes: offset.max(0) as usize,
                    limit: self.spill_cap_off.max(0) as usize,
                },
                format!(
                    "spill slot for n{id} past the frame's spill cap \
                     (frame {} bytes, cap {})",
                    self.frame_size, self.spill_cap_off
                ),
            ));
        }
        // `first_spill > 0` and colours are non-negative, so this cannot be
        // `None` — but the conversion is where the "no zero offsets" invariant
        // is *enforced* rather than assumed, so it is checked, not asserted.
        u32::try_from(offset)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| {
                Bailout::new(BailoutReason::Internal(
                    "ir_lower: spill offset 0 would encode [rbp - 0] (saved caller RBP)",
                ))
            })?;
        Ok(offset)
    }

    /// Infallible façade over [`Self::alloc_slot_checked`] for the ~45
    /// per-opcode emission sites, which are `fn(..) -> ()` and cannot `?`.
    ///
    /// On refusal it latches the structured bailout — so `lower_inner` discards
    /// the artifact and the caller falls back to the single-pass backend — and
    /// returns the poison slot so the doomed walk finishes without panicking.
    /// New code should call `alloc_slot_checked` and propagate.
    pub(in crate::ir_lower) fn alloc_slot(&mut self, id: NodeId) -> i32 {
        match self.alloc_slot_checked(id) {
            Ok(offset) => offset,
            Err(bailout) => {
                self.latch_bailout(bailout);
                self.poison_slot()
            }
        }
    }

    /// LV11-4 (JIT round 19 proposal wave 12, lane lv12): the position of
    /// by-bci point `index` -- the earliest scheduled node that claims its
    /// snapshot (`frame_snapshot`), else the earliest node at its bci, which is
    /// where `snapshot_native` / `bci_native` anchor it -- and every value it
    /// names whose home [`point_home_covers`] rejects there. Latches the
    /// refusal (`ir_lower: deopt point names a home past its range`).
    pub(in crate::ir_lower) fn check_deopt_point_ranges(&self, index: usize, sp: &crate::ir::SafepointSnapshot) {
        let plan = &self.slot_plan;
        let pos_of = |id: usize| plan.pos_of.get(id).copied().flatten();
        let claimed = self
            .graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.frame_snapshot == Some(index as u32))
            .filter_map(|(id, _)| pos_of(id))
            .min();
        let pos = claimed.or_else(|| {
            self.graph
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.bytecode_pc == Some(sp.bci))
                .filter_map(|(id, _)| pos_of(id))
                .min()
        });
        let Some(pos) = pos else {
            return;
        };
        for &v in sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
        {
            if v == NO_NODE || point_home_covers(plan, v as usize, pos) {
                continue;
            }
            self.deopt_point_stale.set(self.deopt_point_stale.get() + 1);
            self.latch_bailout(Bailout::with_context(
                BailoutReason::Internal("ir_lower: deopt point names a home past its range"),
                format!(
                    "point {index} (bci {}) at position {pos} names n{v}",
                    sp.bci
                ),
            ));
        }
    }

    /// Interpreter round i2 wave 64 (lane L2): the token-read guard's check,
    /// called from the operand funnels -- [`Self::slot_of_checked`] (and so
    /// `slot_of`), [`Self::resident_gpr`], [`Self::resident_xmm`],
    /// [`Self::gp_load_value`] and [`Self::fp_load_value`].
    ///
    /// `id` is a read of the node being lowered's operand; when that node names
    /// `id` only in a memory-token slot ([`Self::token_only_inputs`]) the read
    /// has no business existing, and with a trimmed home it reads a word the
    /// colouring may have handed on. Such a read latches
    /// `BailoutReason::Internal("ir_lower: operand read through a memory-token
    /// slot")` -- the compile is refused, the method runs in a lower tier.
    ///
    /// Exempt, by construction of the set rather than by a list: an id the node
    /// ALSO names in a value slot (a real read), and an id whose home the
    /// colouring pinned (its range is the pinned one with or without a trim;
    /// counted in `token_read_pinned_hits`, e.g. the deopt description of a
    /// snapshot that names the token producer). Reads outside a data node's
    /// lowering (phi edge copies, deopt pads) see an empty set.
    ///
    /// A hit prints one `[ir-ls] token-read guard:` line under
    /// `CRATONVM_DBG_IR_LINEAR_SCAN` or `CRATONVM_DBG_IR_LS_TOKEN_TRIM_CENSUS`.
    #[track_caller]
    pub(in crate::ir_lower) fn token_read_check(&self, id: NodeId) {
        if self.token_only_inputs.is_empty() {
            return;
        }
        let Some(&(_, slot, pinned)) = self.token_only_inputs.iter().find(|e| e.0 == id) else {
            return;
        };
        let scope = self.token_scope;
        if pinned {
            self.token_read_pinned_hits
                .set(self.token_read_pinned_hits.get() + 1);
            let mut by_scope = self.token_scope_pinned_hits.get();
            by_scope[scope as usize] += 1;
            self.token_scope_pinned_hits.set(by_scope);
            return;
        }
        self.token_read_refusals
            .set(self.token_read_refusals.get() + 1);
        let mut by_scope = self.token_scope_refusals.get();
        by_scope[scope as usize] += 1;
        self.token_scope_refusals.set(by_scope);
        let site = core::panic::Location::caller();
        // The scope's reader: the data node (`cur_def`), or -- JIT round 19
        // wave 10, i64-L2 -- the terminator / the tile root that installed a
        // `term` / `mir` set.
        let reader = match scope {
            TokenScope::Data => self.cur_def.unwrap_or(NO_NODE),
            TokenScope::Term | TokenScope::Mir => self.token_scope_reader,
        };
        let op = self
            .graph
            .nodes
            .get(reader as usize)
            .map(|n| format!("{:?}", n.op))
            .unwrap_or_default();
        if false
            || crate::regalloc::ls_token_trim_census_enabled()
        {
            eprintln!(
                "[ir-ls] token-read guard: n{reader} {op} reads n{id} through token slot {slot} \
                 (at {}:{}) scope={}",
                site.file(),
                site.line(),
                scope.name(),
            );
        }
        // JIT round 19 proposal wave 11, lane lv11 (TOK10-1): in a test build,
        // `CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD=strict` turns a refusal into
        // a panic naming the funnel, so `cargo test -p cratonvm-jit --lib`
        // under that value fails exactly the tests whose compile read through
        // a token slot (`tools/jit/token-read-guard-lib-tests.sh`). The tests
        // that latch a refusal on purpose pin the guard to `1`.
        #[cfg(test)]
        if token_read_guard_strict() {
            panic!(
                "token-read guard (strict test mode): n{reader} ({op}) reads n{id} through its \
                 token slot {slot} (at {}:{}, scope={})",
                site.file(),
                site.line(),
                scope.name(),
            );
        }
        self.latch_bailout(Bailout::with_context(
            BailoutReason::Internal("ir_lower: operand read through a memory-token slot"),
            format!(
                "n{reader} ({op}) reads n{id} through its token slot {slot} (at {}:{}, scope={})",
                site.file(),
                site.line(),
                scope.name(),
            ),
        ));
    }

    /// The token-only inputs of the nodes `ids` for
    /// [`Self::token_only_inputs`]: every id in a memory-token slot of one of
    /// them that no value slot of any of them names
    /// ([`token_only_inputs_across`]). One node: exactly
    /// [`token_only_inputs_in`] of it.
    pub(in crate::ir_lower) fn token_only_inputs_of(&self, ids: &[NodeId]) -> Vec<(NodeId, usize, bool)> {
        let graph = self.graph;
        let nodes: Vec<&Node> = ids
            .iter()
            .filter_map(|&id| graph.nodes.get(id as usize))
            .collect();
        // TOK10-3: the reader's own position, the point every in-scope read
        // (an operand funnel or the reader's own deopt description) happens at.
        let reader_pos = if self.token_guard_point_range {
            ids.first()
                .and_then(|&r| self.slot_plan.pos_of.get(r as usize).copied().flatten())
        } else {
            None
        };
        let ranges = self.slot_plan.colouring_ranges();
        token_only_inputs_across(&nodes, |inp| {
            let i = inp as usize;
            let class = self.slot_plan.class.get(i).copied().flatten();
            let named = self.token_guard_named.get(i).copied().unwrap_or(false);
            let range = ranges.get(i).copied().flatten();
            let exempt = token_exempt_at(class, named, range, reader_pos);
            if !exempt && token_exempt_at(class, named, range, None) {
                self.token_point_stale.set(self.token_point_stale.get() + 1);
            }
            exempt
        })
    }

    /// Install the token-read guard's set for the lowering scope `scope`
    /// over the nodes `ids` (the first is the scope's reader), returning what
    /// [`Self::token_scope_exit`] restores. `None`, and nothing computed,
    /// while the guard is off -- so a guard-off compile runs exactly the code
    /// it ran before the scopes existed.
    ///
    /// Interpreter round i2 wave 64 (lane L2) built the `data` scope inline in
    /// [`Self::lower_data_node_tracked`]; JIT round 19 wave 10 (lane tok10,
    /// `i64-L2-proposal-the-token-read-guard-covers-terminators-and-fused-compares`)
    /// moved it here and added the `term` and `mir` scopes.
    pub(in crate::ir_lower) fn token_scope_enter(&mut self, ids: &[NodeId], scope: TokenScope) -> Option<TokenScopeSaved> {
        if !self.token_read_guard {
            return None;
        }
        if self.token_guard_named.is_empty() {
            let mut named = vec![false; self.graph.nodes.len().max(1)];
            for sp in &self.graph.safepoints {
                for v in sp
                    .locals
                    .iter()
                    .chain(sp.stack.iter())
                    .chain(sp.monitors.iter())
                    .copied()
                    .chain(sp.sb_virtual_nodes())
                {
                    if let Some(cell) = named.get_mut(v as usize) {
                        *cell = true;
                    }
                }
            }
            self.token_guard_named = named;
        }
        let set = self.token_only_inputs_of(ids);
        if !set.is_empty() {
            self.token_read_nodes += 1;
            self.token_scope_nodes[scope as usize] += 1;
        }
        let saved = TokenScopeSaved {
            set: std::mem::replace(&mut self.token_only_inputs, set),
            scope: self.token_scope,
            reader: self.token_scope_reader,
        };
        self.token_scope = scope;
        self.token_scope_reader = ids.first().copied().unwrap_or(NO_NODE);
        Some(saved)
    }

    /// Undo [`Self::token_scope_enter`].
    pub(in crate::ir_lower) fn token_scope_exit(&mut self, saved: Option<TokenScopeSaved>) {
        if let Some(saved) = saved {
            self.token_only_inputs = saved.set;
            self.token_scope = saved.scope;
            self.token_scope_reader = saved.reader;
        }
    }

    /// The census line's `scope=` field: `name:nodes/refusals/pinned-hits`
    /// per [`TokenScope`], comma-separated.
    pub(in crate::ir_lower) fn token_scope_census(&self) -> String {
        let refusals = self.token_scope_refusals.get();
        let pinned = self.token_scope_pinned_hits.get();
        TokenScope::ALL
            .iter()
            .map(|&sc| {
                let i = sc as usize;
                format!(
                    "{}:{}/{}/{}",
                    sc.name(),
                    self.token_scope_nodes[i],
                    refusals[i],
                    pinned[i]
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The nodes a terminator's lowering reads operands for: the terminator,
    /// and for an `If` whose compare is FUSED into the branch (`fused_cmp`)
    /// that compare and any `lcmp` / `fcmp` fused under it (`fused_lcmp`,
    /// `fused_fcmp`) -- their arms emit nothing and their operands are read at
    /// the branch. JIT round 19 wave 10 (lane tok10, i64-L2).
    pub(in crate::ir_lower) fn term_token_scope_nodes(&self, term: NodeId) -> Vec<NodeId> {
        let mut out = vec![term];
        let Some(node) = self.graph.nodes.get(term as usize) else {
            return out;
        };
        if !matches!(node.op, Op::If) {
            return out;
        }
        let Some(&cond) = node.inputs.get(1) else {
            return out;
        };
        if cond == NO_NODE || !self.fused_cmp.get(cond as usize).copied().unwrap_or(false) {
            return out;
        }
        out.push(cond);
        if let Some(cmp) = self.graph.nodes.get(cond as usize) {
            for &inp in &cmp.inputs {
                if inp == NO_NODE || out.contains(&inp) {
                    continue;
                }
                let fused_under = self.fused_lcmp.get(inp as usize).copied().unwrap_or(false)
                    || self.fused_fcmp.get(inp as usize).copied().unwrap_or(false);
                if fused_under {
                    out.push(inp);
                }
            }
        }
        out
    }

    /// The nodes the level-2 tile rooted at `id` in `block_idx` covers, root
    /// first, when the MIR plan has one (the tile [`Self::mir_tile_bytes`] /
    /// [`Self::mir_shadow_tile_bytes`] would encode). JIT round 19 wave 10.
    pub(in crate::ir_lower) fn mir_tile_covered(&self, block_idx: usize, id: NodeId) -> Option<Vec<NodeId>> {
        let plan = self.mir.as_ref()?;
        let (tb, ti) = (*plan.tile_of.get(id as usize)?)?;
        if usize::try_from(tb).ok()? != block_idx {
            return None;
        }
        let tile = plan
            .blocks
            .get(usize::try_from(tb).ok()?)?
            .tiles
            .get(usize::try_from(ti).ok()?)?;
        let mut out = vec![tile.root];
        out.extend(tile.covered.iter().copied().filter(|&c| c != tile.root));
        Some(out)
    }

    /// The frame offset of a node's result, or a bailout if it has none.
    ///
    /// The only read path into `node_slot`. There is no longer a value that
    /// means "unallocated": an absent location is an `Err`, never `0`.
    #[track_caller]
    pub(in crate::ir_lower) fn slot_of_checked(&self, id: NodeId) -> CompileResult<i32> {
        self.token_read_check(id);
        // A home nobody wrote is not a location. Refusing here fails the
        // compile and drops the method to the single-pass backend — a coverage
        // loss, never a wrong answer — and `home_read_refusals` names the site
        // that wanted converting. See `home_dropped`.
        //
        // "Names the site" was a COUNT until an `Op::NewArray` length read sent
        // `StringRegexOnly.run` to the single-pass backend and the only way to
        // find which of the ~45 emission sites had asked was to rebuild with a
        // backtrace. `#[track_caller]` costs nothing on the success path — the
        // caller's `Location` is a compile-time constant threaded in a register
        // — and turns the refusal into the file:line of the reader that wants
        // converting to `gp_load_value`. Both façades below forward it.
        if self.home_dropped.get(id as usize).copied().unwrap_or(false) {
            self.home_read_refusals
                .set(self.home_read_refusals.get() + 1);
            let site = core::panic::Location::caller();
            return Err(Bailout::with_context(
                BailoutReason::UnallocatedValue { node: id },
                format!(
                    "n{id}'s home word is never written; read it from its register \
                     (read at {}:{})",
                    site.file(),
                    site.line(),
                ),
            ));
        }
        match self.node_slot.get(id as usize).copied().flatten() {
            // `NonZeroU32` ⇒ never 0, and `alloc_slot_checked` bounds it by
            // `spill_cap_off` ⇒ always inside the frame.
            Some(off) => Ok(off.get() as i32),
            None => Err(Bailout::new(BailoutReason::UnallocatedValue { node: id })),
        }
    }

    /// Infallible façade over [`Self::slot_of_checked`] for the emission sites.
    ///
    /// See `unallocated_slot_use`: this path is unreachable now that
    /// `verify_data_locations` refuses such graphs before emission starts. If
    /// it is ever taken, the latch discards the artifact and the caller falls
    /// back to the single-pass backend — the same outcome as before, but with a
    /// structured reason attached, and returning the poison slot rather than
    /// `0` so no `[rbp - 0]` read can be emitted even transiently.
    /// The frame offset a value's home WOULD occupy, whether or not anything
    /// writes it.
    ///
    /// The single exemption from [`Self::slot_of`]'s refusal, and it is narrow
    /// on purpose: `gather_phi_copies` needs the offset to identify a
    /// destination and to sequentialise the edge's parallel copy, and it hands
    /// that offset to `emit_copy_op`, which decides per copy whether to store
    /// through it. Nothing here reads the word.
    #[track_caller]
    pub(in crate::ir_lower) fn slot_of_unwritten(&self, id: NodeId) -> i32 {
        match self.node_slot.get(id as usize).copied().flatten() {
            Some(off) => off.get() as i32,
            None => self.slot_of(id),
        }
    }

    #[track_caller]
    pub(in crate::ir_lower) fn slot_of(&self, id: NodeId) -> i32 {
        match self.slot_of_checked(id) {
            Ok(offset) => offset,
            Err(bailout) => {
                self.unallocated_slot_use.set(true);
                self.latch_bailout(bailout);
                if false {
                    match self.graph.nodes.get(id as usize) {
                        Some(n) => eprintln!(
                            "[irslot] UNALLOCATED node={} op={:?} ty={:?} inputs={:?} pc={:?}",
                            id, n.op, n.ty, n.inputs, n.bytecode_pc
                        ),
                        None => eprintln!("[irslot] UNALLOCATED node={id} (out of range)"),
                    }
                }
                self.poison_slot()
            }
        }
    }

    // `CRATONVM_JIT_IR_RELOC_EMIT=0` — disable the relocation contract's EMISSION
    // side entirely (safepoint-id stores, shadow push/reload, the prologue thread
    // fetch), leaving the frame layout alone.
    //
    // Exists so the cost of that emission can be A/B'd on ONE binary against a
    // deterministic probe. Three attempts inferred it from a single run of a suite
    // class that later turned out to be bimodal; a lever plus repeats is what that
    // should have been. Default on, so this changes nothing unless asked.


    // `CRATONVM_JIT_IR_COLD_ARG_STAGE=0` — stage a call's outgoing arguments
    // EAGERLY, before the call, as this backend did until 2026-09-02.
    //
    // Default on, meaning the staging happens on each reader's own cold side.
    // The switch exists because the eager version was removed without one, and
    // a change to what a frame holds across a call is exactly the kind that
    // has to be A/B-able in ONE binary when a GC-stress test starts failing.
    // Not having it cost a rebuild per hypothesis.


    // ── Moving-young relocation contract ─────────────────────────────────
    //
    // What `conservative_roots` demands of a compiled frame before a young
    // collection may RELOCATE while that frame is live:
    //
    //   1. a non-zero `sp_id_slot_off`, and the id of the active safepoint
    //      stored there, so the exact map can be found (never a union);
    //   2. an `OopMapEntry` whose `bytecode_pc` equals that id and whose
    //      `moving_young_coverage_complete` is set. **No matching entry is a
    //      refusal**, not an absence of objections;
    //   3. `frame_slot_offsets` naming every frame slot that holds a live
    //      reference, so each can be rewritten in place;
    //   4. a `frame_layout` + `live_frame_hi` precise enough that the band scan
    //      can tell storage this frame resumes from apart from dead spill and
    //      outgoing-argument scratch.
    //
    // The IR lowerer is unusually well placed to satisfy this: every REFERENCE
    // keeps an authoritative home word (`value_home_droppable` admits only
    // `Int`/`Long`; the opt-in `ir_ref_residency_enabled` caches a reference in
    // a register but invalidates the copy at every GC point), so unlike the
    // single-pass backend there are no register-resident oops to chase — the
    // complete root set at a safepoint is a set of frame offsets. (Primitive
    // values may live in registers since linear scan went default-ON; that
    // does not touch the root set.)

    /// Interpreter round i2 wave 58 (lane L2,
    /// `CRATONVM_JIT_IR_SYNC_SPLICE_ENTER_NAMES_CALLEE`): when `id` is the
    /// `MonitorEnter` of a synchronized splice window
    /// (`Graph::sync_splice_windows`), the combined pc of that spliced callee's
    /// first instruction -- `chain_at` of it is the callee at bci 0 over its
    /// callers. `None` with the switch off, the inline-frame map off, for
    /// every other monitor op, and when the window's invoke spliced no body
    /// or more than one (a bimorphic site), so a chain is never guessed.
    ///
    /// Why: HotSpot builds a synchronized method's frame before
    /// `lock_method`, so a thread blocked entering it lists it on top
    /// (`docs/feature-designs/jit-ir-optimizations.md`).
    /// The splice takes the callee's monitor in the caller's frame, at the
    /// invoke's pc; without this the blocked frame read `spin` at the invoke.
    /// Compile time only: the map is published on the slow path of the
    /// acquire, the inline thin lock takes no map at all.
    pub(in crate::ir_lower) fn sync_splice_enter_callee_base(&self, id: NodeId) -> Option<usize> {
        if self.inline_frame_sites.is_empty()
            || !crate::x64::inline_frame_map_enabled()
            || !self.graph.sync_splice_windows.iter().any(|w| w.enter == id)
        {
            return None;
        }
        let pc = self.graph.nodes.get(id as usize)?.bytecode_pc?;
        self.inline_frame_sites.sole_body_base_for_caller_pc(pc)
    }

    /// Record a TWIN of the safepoint map `a` that [`Self::emit_safepoint_map`]
    /// just recorded: the same oop map, the same id-to-bci row and liveness
    /// claim, under a new id whose inline chain is the one at `cur_node_pc`
    /// (the caller's own). Returns the twin's id for the caller to store after
    /// the call that map protected returned, or `None` when `a` is not the
    /// last map recorded (nothing to copy). Interpreter round i2 wave 58,
    /// lane L2: a synchronized splice's enter names its callee only while the
    /// acquire runs.
    pub(in crate::ir_lower) fn record_safepoint_twin(&mut self, a: u32) -> Option<u32> {
        let map = self.oop_maps.last().filter(|m| m.bytecode_pc == a)?.clone();
        let twin = self.next_sp_id;
        self.next_sp_id = self.next_sp_id.wrapping_add(1);
        let mut map = map;
        map.bytecode_pc = twin;
        map.native_pc_offset = 0;
        self.oop_maps.push(map);
        // GC719-1 (round 19 wave 8, lane gc8): the twin holds what `a` holds.
        if let Some(homes) = self
            .ir_monitor_homes
            .last()
            .filter(|(id, _)| *id == a)
            .map(|(_, homes)| homes.clone())
        {
            self.ir_monitor_homes.push((twin, homes));
        }
        if let Some(&(_, bci)) = self.sp_id_bcis.iter().rev().find(|&&(id, _)| id == a) {
            self.sp_id_bcis.push((twin, bci));
        }
        if let Some(rec) = self
            .frame_liveness_records
            .iter()
            .rev()
            .find(|r| r.sp_id == a)
            .cloned()
        {
            self.frame_liveness_records
                .push(crate::SafepointFrameLiveness { sp_id: twin, ..rec });
        }
        if !self.inline_frame_sites.is_empty() && crate::x64::inline_frame_map_enabled() {
            let chain: Vec<crate::x64::InlineFrameLevel> = match self.cur_node_pc {
                Some(node_pc) => self
                    .inline_frame_sites
                    .chain_at(node_pc)
                    .into_iter()
                    .map(|l| crate::x64::InlineFrameLevel {
                        label: l.method_key.into(),
                        bci: l.bci,
                        class_id: l.class_id,
                        // Wave 122 (L2): the optimizing tier stamps no bit.
                        hidden: None,
                    })
                    .collect(),
                None => Vec::new(),
            };
            self.inline_frame_sp_id_rows.push((twin, chain));
        }
        Some(twin)
    }

    /// `MOV qword [rbp - sp_id_slot_off], imm32`: publish `id` as this frame's
    /// active safepoint. Sign-extended; ids are small.
    pub(in crate::ir_lower) fn emit_sp_id_store(&mut self, id: u32) {
        self.buf.emit(&[0x48, 0xC7, 0x85]);
        self.buf.emit(&(-self.sp_id_slot_off).to_le_bytes());
        self.buf.emit(&id.to_le_bytes());
    }

    /// `CRATONVM_JIT_IR_SELF_CALL_LAZY_SP_ID=0` -- keep the safepoint-id store on
    /// the hot path of a direct self-call in a reference-free body (round 11
    /// wave 15, lane fib; step 3 of `r11w14-rt-ir-lower-oop-free-frame-patch`).
    /// Default on: the store moves to the call's two cold paths, see
    /// `deferred_self_call_sp_id`. Read per compile, so a test can arrange it.


    /// gen r5w2/oomjit6: the `(plan position, method bci, node)` that arms
    /// [`Self::emit_dead_ref_slot_clears`] while node `id` is lowered, or
    /// `None`. An `Op::Call` qualifies (by default the only op that does): it
    /// is control-pinned, so its bci is
    /// its program point, which is what the reachability argument in the
    /// section above [`DeadRefSlotClear`] needs.
    ///
    /// gen r5w5/oomjit9: a call inside a splice qualifies too. It used to be
    /// left alone because its bci is a combined-buffer pc, not one of this
    /// method's -- and that made every GC point reached through an inlined
    /// callee (`cleared(ref)`'s `System.gc()` spliced into an OSR body, the
    /// shape of `GenR5W3OsrHolderProbe`) a site where nothing was ever
    /// claimed. The bci is now the OUTERMOST enclosing invoke of this method's
    /// own code ([`resolve_spliced_pc`]), and the keep set at that bci
    /// includes every snapshot inside every splice that resolves to an
    /// invoke reachable from it ([`DeadRefSlotClear::splice_named`]), so all
    /// that can run after the call -- the rest of the splice and of the
    /// method -- is covered.
    ///
    /// JIT round 20 wave 17, lane gcDead (I-9): an `Op::New` / `Op::NewArray`
    /// qualifies too under [`Self::alloc_dead_home_clear`]. Both are
    /// control-pinned like a call, and their runtime helper is a GC point;
    /// see the section above [`ir_alloc_dead_home_clear_enabled`] for where
    /// their clears run and why the site never counts as cleaning.
    ///
    /// gci i1/fr (`CRATONVM_JIT_IR_ALLOC_SITE_CLEARS`, default ON since gci i3;
    /// `gengc-r5w5-oomjit9-...` item 1): an `Op::New` / `Op::NewArray` too
    /// ([`dead_ref_clear_arms`]). Both are control-pinned exactly like a call
    /// (`ir_schedule::pinned_anchor`), and the keep-set builders record their
    /// after-sets under the same switch, so the collection an allocation's
    /// slow path reaches sees the homes dead there cleared and claimed dead,
    /// as a call's GC point does.
    ///
    /// At the 2026-10-15 merge of dev into the gci round the two rules
    /// compose: an allocation is armed only while BOTH
    /// `CRATONVM_JIT_IR_ALLOC_DEAD_HOME_CLEAR` (dev; dropped when no plan
    /// converged) and `CRATONVM_JIT_IR_ALLOC_SITE_CLEARS` (gci) are on, and its
    /// clears are placed by the I-9 slow-path machinery; the pinned non-call GC
    /// points (gci i4/ir, `CRATONVM_JIT_IR_OTHER_GC_POINT_CLEARS`) stay armed
    /// under their own switch.
    pub(in crate::ir_lower) fn dead_ref_clear_site_for(&self, id: NodeId) -> Option<(usize, usize, NodeId)> {
        if self.dead_ref_clear.is_none() {
            return None;
        }
        let node = self.graph.nodes.get(id as usize)?;
        let armed = match node.op {
            Op::Call { .. } => true,
            Op::New { .. } | Op::NewArray { .. } => {
                self.alloc_dead_home_clear && self.alloc_site_clears
            }
            // r23 wave 2d, lane L8: a `getstatic` the lowering reads DIRECTLY
            // (`emit_inline_getstatic`: two plain loads, no helper, no
            // `<clinit>`, no map) is no GC point at all, so arming it bought
            // only its first-byte clears -- nine stores per iteration in
            // `R23W2cAbsOnly.absGet`'s loop, twice.
            Op::LoadStatic {
                class_id,
                field_index,
                type_tag,
                ..
            } if self.load_static_reads_directly(class_id, field_index, type_tag) =>
            {
                false
            }
            _ => self.other_site_clears && other_gc_point_is_pinned_site(self.graph, node),
        };
        if !armed {
            return None;
        }
        let pc = node.bytecode_pc?;
        let bci = resolve_spliced_pc(self.spliced_ranges, pc);
        let pos = self.slot_plan.pos_of.get(id as usize).copied().flatten()?;
        Some((pos, bci, id))
    }

    /// gci i5/ir (`CRATONVM_JIT_IR_POLL_GC_POINT_CLEARS`, opt-in): the armed
    /// site of the back-edge poll at the end of `block`, or `None` (arm
    /// nothing) off the switch, without the facts, or for a poll
    /// [`back_edge_poll_header`] does not arm. The position is the
    /// terminator's when the block has one (the poll precedes its compare),
    /// else the block's edge position; the bci is the loop header's, out of a
    /// splice as a call's is; the site is [`poll_site_key`]. See the section
    /// above [`ir_poll_gc_point_clears_enabled`].
    pub(in crate::ir_lower) fn poll_dead_ref_clear_site(&self, block: usize) -> Option<(usize, usize, NodeId)> {
        if self.poll_edge_pos.is_empty() || self.dead_ref_clear.is_none() {
            return None;
        }
        let (_, header_pc) = back_edge_poll_header(self.graph, self.schedule, block)?;
        let edge = self.poll_edge_pos.get(block).copied()?;
        let pos = match self.schedule.blocks.get(block)?.terminator {
            Some(term) => self.slot_plan.pos_of.get(term as usize).copied().flatten()?,
            None => edge,
        };
        let bci = resolve_spliced_pc(self.spliced_ranges, header_pc);
        let site = poll_site_key(self.graph, block)?;
        Some((pos, bci, site))
    }

    /// gci i5/ir: emit an armed back-edge poll's map (`block` = the poll's
    /// back-edge block, `None` at the method entry, which is never armed) with
    /// [`Self::poll_dead_ref_clear_site`] installed around it, and the dense
    /// stores forced. The poll carries the returned map through its own call.
    pub(in crate::ir_lower) fn emit_poll_safepoint_map(&mut self, block: Option<usize>) -> Option<SafepointMapToken> {
        if !Self::ir_gc_point_maps_enabled() || self.sp_id_slot_off <= 0 || !self.reloc_emit_on {
            return None;
        }
        if self.defer_next_sp_id_store {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "IR poll cannot defer its safepoint ID store",
            )));
            return None;
        }
        let site = block.and_then(|b| self.poll_dead_ref_clear_site(b));
        let Some((pos, bci, key)) = site else {
            return self.emit_safepoint_map(self.spill_high_water);
        };
        if false {
            eprintln!(
                "[ir-slots] poll site armed: block={} pos={pos} bci={bci} site=n{key}",
                block.unwrap_or(usize::MAX)
            );
        }
        let saved_site = std::mem::replace(&mut self.dead_ref_clear_site, site);
        let saved_args = std::mem::replace(&mut self.dead_ref_clear_site_args, usize::MAX);
        let saved_dense = std::mem::replace(&mut self.poll_site_dense, true);
        let map = self.emit_safepoint_map(self.spill_high_water);
        self.dead_ref_clear_site = saved_site;
        self.dead_ref_clear_site_args = saved_args;
        self.poll_site_dense = saved_dense;
        map
    }

    /// hga (`r16-codegen-hashmap-get-anatomy`): may the `Op::Call` `id` take
    /// one of the inline prefixes its arm tries ahead of the call
    /// (`try_emit_ir_arraylist_access_prefix`, `..._box_hash_equals_prefix`,
    /// `..._integer_value_of_prefix`, `..._long_value_of_prefix`)? The
    /// method-name screens of those four only, so a superset: a `true` here
    /// for a call no prefix claims merely keeps more homes "may be stale" in
    /// [`stale_sets_at_sites`], which can only add stores, never drop one.
    /// Always `false` under `CRATONVM_JIT_IR_PREFIX_SLOW_PATH_CLEARS=0`.
    pub(in crate::ir_lower) fn call_site_may_take_inline_prefix(&self, id: NodeId) -> bool {
        self.prefix_slow_path_clears && self.call_site_screens_admit_inline_prefix(id)
    }

    /// The site screens of [`Self::call_site_may_take_inline_prefix`],
    /// whatever `CRATONVM_JIT_IR_PREFIX_SLOW_PATH_CLEARS` says: may an inline
    /// prefix of its arm claim the `Op::Call` `id`? A superset. Every prefix
    /// declines a call whose result home is dropped ("a carried or dropped
    /// result home"), so the register allocator asks this before it drops one
    /// (`r25_w7a_reg_alloc`, `Lowerer::reg_alloc_home_is_one_store`).
    pub(in crate::ir_lower) fn call_site_screens_admit_inline_prefix(&self, id: NodeId) -> bool {
        match self.graph.nodes.get(id as usize).map(|n| &n.op) {
            Some(&Op::Call { info_ptr }) => self.call_info_admits_inline_prefix(info_ptr),
            _ => false,
        }
    }

    /// [`Self::call_site_screens_admit_inline_prefix`] for the call whose
    /// `JitInvokeInfo` is `info_ptr` (`0`: none, so no prefix).
    pub(in crate::ir_lower) fn call_info_admits_inline_prefix(&self, info_ptr: usize) -> bool {
        if info_ptr == 0 {
            return false;
        }
        // SAFETY: as every other read of an `Op::Call`'s `info_ptr` in this
        // file -- a live `JitInvokeInfo` owned by `ir_call_infos`.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        let (class, name, desc) = (info.class_name, info.method_name, info.descriptor);
        let box_value_of = info.invoke_kind == 3
            && name == "valueOf"
            && matches!(
                (class, desc),
                ("java/lang/Integer", "(I)Ljava/lang/Integer;")
                    | ("java/lang/Long", "(J)Ljava/lang/Long;")
            );
        let hash_equals = info.invoke_kind == 0
            && ((name == "hashCode" && desc == "()I")
                || (name == "equals" && desc == "(Ljava/lang/Object;)Z"));
        let array_list = (self.array_list_layout.is_some()
            && matches!(info.invoke_kind, 0 | 2)
            && crate::try_resolve_arraylist_intrinsic(class, name, desc, self.array_list_layout)
                .is_some())
            // Round rx27 wave 3 (lane splitfast): the `add` prefix
            // (`try_emit_ir_arraylist_add_prefix`); its site screen only.
            || (self
                .array_list_layout
                .is_some_and(|al| al.mod_count_compact_offset.is_some())
                && crate::ir_arraylist_add_prefix_enabled()
                && crate::arraylist_add_site(class, name, desc, info.invoke_kind));
        // nio20 L3: the `getClass()` prefix (`try_emit_ir_getclass_prefix`).
        let get_class = self.getclass_prefix_admits(info);
        // nio20 L2 (NIO19-2): the `Unsafe` accessor prefix
        // (`try_emit_ir_unsafe_access_prefix`); its method screen only.
        // nio21 wave 5 L3: and its CAS / getAndAdd / getAndSet rows
        // (`try_emit_ir_unsafe_rmw_prefix`).
        let unsafe_access = self.direct_helpers.unsafe_inline_admitted != 0
            && (crate::direct_helpers::unsafe_inline_access_site(class, name, desc).is_some()
                || crate::direct_helpers::unsafe_inline_rmw_site(class, name, desc).is_some()
                // JIT round 20 wave 12, lane qUnsafe (AT-4): the reference
                // CAS row (`try_emit_ir_unsafe_ref_cas_prefix`).
                || crate::direct_helpers::unsafe_inline_ref_cas_site(class, name, desc)
                // JIT round 21 wave 1, lane updaters (UPDATERS21-1): the
                // reference volatile get / put rows.
                || crate::direct_helpers::unsafe_inline_ref_volatile_site(class, name, desc)
                    .is_some()
                // JIT round 21, lane atomics (UNSAFE21-2): the sub-word CAS
                // rows (`try_emit_ir_unsafe_subword_prefix`).
                || crate::direct_helpers::unsafe_inline_subword_site(class, name, desc).is_some());
        // nio20 wave 3 L2: the String search prefix
        // (`try_emit_ir_string_search_prefix`); its site screen only. Invoke
        // kind 1 too, which `hash_equals` above does not cover.
        let string_search = self.string_layout.is_some()
            && crate::ir_string_search_prefix_kind(class, name, desc)
                .is_some_and(|k| k.admits_invoke_kind(info.invoke_kind));
        // nio20 wave 5 L2: the identity-hash prefix
        // (`try_emit_ir_identity_hash_prefix`); its site screen only.
        let identity_hash = self.identity_hash_prefix_admits(info).is_some();
        // nio21 wave 1 L5: the current-thread prefix
        // (`try_emit_ir_current_thread_prefix`).
        let current_thread = self.current_thread_prefix_admits(info);
        // nio21 wave 11 L5: the constant-`VarHandle` prefix
        // (`try_emit_ir_vh_const_prefix`); its site screen only.
        // JIT r20 wave 18 (lane xVhArr): and its array-element arm
        // (`try_emit_ir_vh_const_array_prefix`).
        let vh_const = class == "java/lang/invoke/VarHandle"
            && vh_const_inline_enabled()
            && (vh_const_site_shape(name, desc).is_some()
                || vh_const_array_site_shape(name, desc).is_some()
                // j27 wave 2 (lane VHCOMP): and its static-field arm
                // (`vh_static_const::try_emit_ir_vh_const_static_prefix`).
                || vh_static_const::vh_static_site_shape(name, desc).is_some());
        // JIT round 20 wave 17, lane pAtom2: the `Class.isInstance` prefix
        // (`try_emit_ir_class_is_instance_prefix`).
        let class_is_instance = self.class_is_instance_prefix_admits(info);
        // JIT round 20 wave 19, lane zAssign: the `Class.isAssignableFrom`
        // prefix (`try_emit_ir_class_is_assignable_prefix`).
        let class_is_assignable = self.class_is_assignable_prefix_admits(info);
        // JIT r23 wave 8, lane L8: the `Array.newArray` prefix
        // (`try_emit_ir_reflect_new_array_prefix`), and the reference
        // `System.arraycopy` prefix of wave 6 (`try_emit_ir_ref_arraycopy_prefix`),
        // which was not listed here; their site screens only.
        let reflect_new_array = ir_inline_reflect_new_array_enabled()
            && reflect_new_array_prefix_site(class, name, desc, info.invoke_kind);
        let ref_arraycopy = info.invoke_kind == 3
            && class == "java/lang/System"
            && name == "arraycopy"
            && desc == "(Ljava/lang/Object;ILjava/lang/Object;II)V";
        let off_heap_copy = (class == "jdk/internal/misc/ScopedMemoryAccess"
            && name == "copyMemoryInternal"
            && desc == "(Ljdk/internal/foreign/MemorySessionImpl;Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;JLjava/lang/Object;JJ)V")
            || (class == "jdk/internal/misc/Unsafe"
                && name == "copyMemory0"
                && desc == "(Ljava/lang/Object;JLjava/lang/Object;JJ)V");
        // j27 wave 4, lane CLONEIR: the array `clone()` prefix
        // (`clone_inline::try_emit_ir_array_clone_prefix`); its site screen only.
        let array_clone =
            clone_inline::array_clone_site_shape(class, name, desc, info.invoke_kind).is_some();
        let byte_array_view =
            info.invoke_kind == 3 && byte_array_view_site_shape_parts(class, name, desc).is_some();
        let hashmap_get = crate::ir::ir_hashmap_get_inline_enabled()
            && !cratonvm_types::narrow_oops_enabled()
            && (class == "java/util/HashMap" || class == "java/util/Map")
            && name == "get"
            && desc == "(Ljava/lang/Object;)Ljava/lang/Object;"
            && matches!(info.invoke_kind, 0 | 1 | 2);
        reflect_new_array
            || array_clone
            || byte_array_view
            || hashmap_get
            || ref_arraycopy
            || off_heap_copy
            || class_is_instance
            || class_is_assignable
            || vh_const
            || box_value_of
            || hash_equals
            || array_list
            || get_class
            || unsafe_access
            || string_search
            || identity_hash
            || current_thread
    }

    /// hga: the planned clears of the `Op::Call` being lowered, held from its
    /// first byte to the end of its inline prefixes, then emitted there --
    /// at the head of the call path, ahead of its safepoint map and its
    /// argument staging. Every decline edge of a prefix lands on that point,
    /// and no prefix's hit path contains a GC point (they load, compare,
    /// bump a TLAB and store the result home; none calls out), so each GC
    /// point the call reaches still finds the planned words zero, while a
    /// prefix hit skips the stores. The planner knows the hit path does not
    /// zero ([`Self::call_site_may_take_inline_prefix`]).
    pub(in crate::ir_lower) fn emit_pending_call_clears(&mut self) {
        if std::mem::take(&mut self.pending_arg_stage_clears) {
            self.emit_dead_arg_stage_clears();
        }
        if let Some(offs) = self.pending_call_clears.take() {
            self.emit_dead_home_clear_batch(&offs);
        }
    }

    /// gci i3/ir (opt-in `CRATONVM_JIT_IR_REF_PARAM_HOME_ZERO`; the section
    /// above [`ref_param_home_zero_candidates`]): right after reference
    /// parameter `id` was copied from its prologue word `[rbp -
    /// param_offset]` into its own home `[rbp - slot]`, zero the prologue
    /// word, so the parameter's only frame copy is the coloured one that the
    /// dead-home clears and the liveness claim already govern.
    ///
    /// Declines (writes nothing) for a node the plan did not admit, a copy
    /// that IS the prologue word, a copy whose store was dropped (the
    /// register is then the value's only location and nothing else would
    /// hold it in the frame), and the self-locking body's receiver (its
    /// exceptional exits release the monitor from that word). `MOV qword
    /// [rbp - off], 0`: no register, no flag, so a carry out of this node is
    /// undisturbed.
    pub(in crate::ir_lower) fn zero_ref_param_home_after_copy(&mut self, id: NodeId, slot: i32, param_offset: i32) {
        if self.ref_param_home_zero.binary_search(&id).is_err() {
            return;
        }
        if slot <= 0
            || slot == param_offset
            || param_offset <= 0
            || param_offset >= self.first_spill
            || self.home_dropped.get(id as usize).copied().unwrap_or(false)
            || self.self_lock_obj_home() == Some(param_offset)
        {
            return;
        }
        self.emit_zero_frame_slot(param_offset);
        if false {
            eprintln!(
                "[ir-slots] ref param home zeroed after its copy: n{id} off={param_offset} copy={slot}"
            );
        }
    }

    /// gci i5/ir (`CRATONVM_JIT_IR_REF_PARAM_HOME_ZERO_LAZY`, opt-in; the
    /// section above [`ir_ref_param_home_zero_lazy_enabled`]): after
    /// [`Self::plan_dead_home_clears`] and the candidate plan, move each
    /// candidate's prologue-word zeroing into the planned first-byte clears of
    /// its own colour, and drop it from [`Self::ref_param_home_zero`] (no
    /// store after the copy). A candidate the rule cannot place keeps the
    /// eager store. Declines, as the eager store does, a copy that is the
    /// prologue word, a dropped home and the self-locking receiver's word;
    /// and needs a complete plan and a colour no other node shares.
    pub(in crate::ir_lower) fn plan_lazy_ref_param_home_zero(&mut self) {
        if self.ref_param_home_zero.is_empty() || !self.dead_home_clears_complete {
            return;
        }
        let graph = self.graph;
        let schedule = self.schedule;
        let slot_plan = self.slot_plan;
        let first_spill = self.first_spill;
        let spill_cap_off = self.spill_cap_off;
        let self_lock_home = self.self_lock_obj_home();
        let nb = schedule.blocks.len();
        let mut params: Vec<(NodeId, i32, i32, usize, usize)> = Vec::new();
        for &id in &self.ref_param_home_zero {
            let Some(Op::Param(idx)) = graph.nodes.get(id as usize).map(|n| &n.op) else {
                continue;
            };
            let param_off = ((*idx as i32) + 1) * 8;
            let Some(Some(colour)) = slot_plan.node_color.get(id as usize).copied() else {
                continue;
            };
            if slot_plan
                .node_color
                .iter()
                .filter(|&&k| k == Some(colour))
                .count()
                != 1
            {
                continue;
            }
            let Some(home_off) = i32::try_from(u64::from(colour).saturating_mul(8))
                .ok()
                .and_then(|delta| first_spill.checked_add(delta))
                .filter(|&off| off > 0 && off <= spill_cap_off)
            else {
                continue;
            };
            if param_off <= 0
                || param_off >= first_spill
                || home_off == param_off
                || self.home_dropped.get(id as usize).copied().unwrap_or(false)
                || self_lock_home == Some(param_off)
            {
                continue;
            }
            let Some(block) = schedule
                .node_to_block
                .get(id as usize)
                .copied()
                .filter(|&b| b < nb)
            else {
                continue;
            };
            let Some(pos) = slot_plan.pos_of.get(id as usize).copied().flatten() else {
                continue;
            };
            params.push((id, param_off, home_off, block, pos));
        }
        let (adds, lazy) = lazy_ref_param_home_zero_plan(
            &params,
            &self.dead_home_clears,
            |site| {
                let b = schedule
                    .node_to_block
                    .get(site as usize)
                    .copied()
                    .filter(|&b| b < nb)?;
                let p = slot_plan.pos_of.get(site as usize).copied().flatten()?;
                Some((b, p))
            },
            |a, b| schedule.dom.dominates(a, b),
        );
        let dbg = false;
        for &(site, off) in &adds {
            if let Some(offs) = self.dead_home_clears.get_mut(&site) {
                offs.push(off);
                offs.sort_unstable();
                offs.dedup();
            }
            if dbg {
                eprintln!("[ir-slots] ref param home zero lazy: off={off} at n{site}");
            }
        }
        if dbg {
            eprintln!(
                "[ir-slots] ref param home zero lazy plan: lazy={lazy:?} sites={}",
                adds.len()
            );
        }
        // gkr: the colour is also zeroed by clears this plan does not list (a
        // loop's entry, a back-edge poll); each lazy parameter's word goes
        // with its colour at every one of them (`emit_dead_home_clear`).
        self.lazy_param_homes = params
            .iter()
            .filter(|p| lazy.binary_search(&p.0).is_ok())
            .map(|&(id, param_off, home_off, _, _)| (id, home_off, param_off))
            .collect();
        self.ref_param_home_zero
            .retain(|id| lazy.binary_search(id).is_err());
    }

    /// JIT round 20 wave 17, lane gcDead (I-9): is `id` an allocation node
    /// armed as a dead-home site right now? Its clears are then held (like a
    /// prefixed call's) for the allocation arm to place on its slow path.
    ///
    /// JIT round 21, lane oom: an armed `Op::MonitorEnter` holds them the same
    /// way, for its slow path (in front of its map, after the inline thin
    /// lock), so the uncontended enter carries no store. `Op::NewCp` is all
    /// slow path and does not hold: its clears run at its first byte, as a
    /// call's do.
    pub(in crate::ir_lower) fn alloc_site_armed(&self, id: NodeId) -> bool {
        self.dead_ref_clear_site
            .is_some_and(|(_, _, site)| site == id)
            && self.graph.nodes.get(id as usize).is_some_and(|n| {
                matches!(
                    n.op,
                    Op::New { .. } | Op::NewArray { .. } | Op::MonitorEnter
                )
            })
    }

    /// JIT round 20 wave 17, lane gcDead (I-9): an allocation route whose
    /// runtime helper call this arm cannot reach separately (it sits inside
    /// a fast-path emitter that also emits the stub) gives up the site
    /// instead: the held clears are dropped AND the site is disarmed before
    /// its map, so that map makes no liveness claim (`precise_liveness_claim`)
    /// and no dense clear -- byte for byte the map it had before I-9. A claim
    /// without the zeroing would leave a dead home out of the precise roots
    /// while it still holds a reference, which a later, unarmed map would
    /// publish after a moving collection had left it stale. Sound because the
    /// stale-set union never counts an allocation site as cleaning.
    pub(in crate::ir_lower) fn disarm_alloc_site(&mut self, id: NodeId) {
        if self.alloc_site_armed(id) {
            self.pending_call_clears = None;
            self.pending_arg_stage_clears = false;
            self.dead_ref_clear_site = None;
        }
    }

    /// JIT round 20 wave 17, lane gcDead (I-9): the primitive-array bump of
    /// an armed `Op::NewArray`, with its slow path landed HERE -- the held
    /// dead-home clears, then the same stub `emit_inline_tlab_newarray_ir`
    /// would have emitted there (same target, atype immediate, length word
    /// and frame record), so the bump path carries no store. Returns the
    /// stub's return address, or `None` having emitted nothing when the bump
    /// declines (the caller then emits the clears and the stub alone).
    pub(in crate::ir_lower) fn emit_newarray_bump_with_slow_path_clears(
        &mut self,
        plan: &crate::runtime_lowering::InlineTlabArrayPlan,
        target: usize,
        immediate: u32,
        length_slot: i32,
        // JIT r23 wave 12 (lane L3, `CRATONVM_JIT_IR_NEWARRAY_FAST_JOIN_ANY_GC`):
        // receives the fast path's jump instead of landing it after the stub,
        // when `gen_newarray_fast_join_admitted` says so.
        fast_join: &mut Option<usize>,
    ) -> Option<usize> {
        let mut slow: Vec<usize> = Vec::new();
        // ZGC: the inline start-bit arm made no call, so (no shadow push
        // pending) it may land at `allocated` past the identity republish
        // and the zero test, as on the unarmed route.
        let zgc_join = self.zgc_inline_arm_fast_join_admitted(plan.announce);
        let mut inline_join: Option<usize> = None;
        let emitted = if zgc_join {
            crate::runtime_lowering::emit_inline_tlab_newarray_fast_path_ir_join(
                &mut self.buf,
                plan,
                target,
                self.frame_record,
                &mut slow,
                &mut inline_join,
            )
        } else {
            crate::runtime_lowering::emit_inline_tlab_newarray_fast_path_ir(
                &mut self.buf,
                plan,
                target,
                self.frame_record,
                &mut slow,
            )
        };
        if !emitted {
            return None;
        }
        let done = self.emit_jmp_rel32();
        for patch in slow {
            self.patch_rel32_to_here(patch);
        }
        self.emit_pending_call_clears();
        let slow_return = crate::runtime_lowering::emit_new_array_stub(
            &mut self.buf,
            self.context_slot_off,
            target,
            immediate,
            length_slot,
            self.frame_record,
        );
        // Wave 12: the armed site is where `R21AllocLean`'s lean loops land
        // (their previous iteration's dead home is cleared on the slow path),
        // so the wave-11 fast join never reached them. The fast path made no
        // call and holds the bumped array in RAX, as on the unarmed route.
        if self.gen_newarray_fast_join_admitted(plan.announce) {
            *fast_join = Some(done);
        } else {
            self.patch_rel32_to_here(done);
            if inline_join.is_some() {
                *fast_join = inline_join;
            }
        }
        Some(slow_return)
    }

    /// JIT round 22 wave 1, lane memory (ALLOC21-1 of
    /// `jit-r21-alloc-tier1-proposals-RETIRED-20261025.md`, [`ir_newarray_map_sink_enabled`], opt-in):
    /// the whole `Op::NewArray` node with its safepoint map on the SLOW path
    /// only, `Op::New`'s `CRATONVM_JIT_IR_ALLOC_MAP_SINK` shape for arrays.
    ///
    /// On a collector that parses the chunk (and, wave 2, on ZGC: see below): there the
    /// inline bump makes no call between the node's start and its result, so
    /// no collector can observe this frame on the fast path and the map (in a
    /// frame-block body the block's first publication) was paid by every
    /// allocation for nothing. The slow path is ordered as the default arm
    /// orders the whole node: held clears (none: an armed I-9 site is
    /// refused here and keeps its own route), the map, the stub, the shadow
    /// reload that pairs with the map's push. Everything is emitted BEFORE
    /// `alloc_slot`, so this node's unwritten result slot is not in the map.
    ///
    /// The length is staged into its frame word (the phi-copy scratch word for
    /// a register-only length) BEFORE the fast path: the map's shadow push
    /// clobbers RAX, not that word, and both paths read the same word.
    ///
    /// Returns `true` having lowered the node (result stored), `false` when
    /// the shape is not admitted or the bump declines; the caller then runs
    /// the default arm (a decline after the staging leaves only that staging
    /// store, which the default arm repeats after its map).
    pub(in crate::ir_lower) fn try_lower_newarray_map_sink(
        &mut self,
        id: NodeId,
        len: NodeId,
        element_type: u8,
        component_class_id: u32,
    ) -> bool {
        // JIT round 22 wave 2 (MEMORY22-1, `CRATONVM_JIT_IR_ALLOC_MAP_SINK_ZGC`,
        // opt-in): on ZGC too, with the announce on the fast path. Its only
        // call is the thin announce helper, a leaf (see the `Op::New` arm's
        // `deferred_zgc`). The split arm keeps its own route.
        let zgc = cratonvm_types::jit_tlab_registration_required();
        if !ir_newarray_map_sink_enabled()
            || !ir_inline_tlab_enabled()
            || self.shadow_thread_slot_off <= 0
            || self.tlab_cursor_off == self.tlab_end_off
        {
            return false;
        }
        if zgc {
            if !ir_alloc_map_sink_zgc_enabled()
                || !crate::x64::zgc_inline_array_announce_enabled()
                || self.zgc_note_tlab_object == 0
            {
                return false;
            }
        } else if false {
            return false;
        }
        let ref_array = element_type == 0;
        if ref_array
            && !((zgc || crate::runtime_lowering::ir_inline_anewarray_any_gc_enabled())
                && crate::x64::inline_tlab_anewarray_enabled())
        {
            return false;
        }
        let (target, immediate) = if ref_array {
            (self.anewarray_object, component_class_id)
        } else {
            (self.newarray, u32::from(element_type))
        };
        if target == 0 {
            return false;
        }
        let length_slot = if self
            .home_dropped
            .get(len as usize)
            .copied()
            .unwrap_or(false)
        {
            self.gp_load_value(RAX, len);
            self.store_rax(self.phi_copy_scratch_slot_off);
            self.phi_copy_scratch_slot_off
        } else {
            self.slot_of(len)
        };
        let array_plan = crate::runtime_lowering::InlineTlabArrayPlan {
            thread_slot_off: self.shadow_thread_slot_off,
            thread_tls_disp: self.ir_tlab_thread_tls_disp(),
            cursor_off: self.tlab_cursor_off,
            end_off: self.tlab_end_off,
            context_off: self.context_slot_off,
            atype: element_type,
            length_off: length_slot,
            announce: if zgc { self.zgc_note_tlab_object } else { 0 },
            component_class_id: ref_array.then_some(component_class_id),
            // As the default arm: a constant length, baked only on ZGC.
            const_length: match self.graph.nodes.get(len as usize).map(|n| &n.op) {
                Some(Op::Const(v)) => i32::try_from(*v).ok().and_then(|v| u32::try_from(v).ok()),
                _ => None,
            },
        };
        let mut slow: Vec<usize> = Vec::new();
        let mut zgc_fast_join: Option<usize> = None;
        let fast_ok = if self.zgc_inline_arm_fast_join_admitted(array_plan.announce) {
            crate::runtime_lowering::emit_inline_tlab_newarray_fast_path_ir_join(
                &mut self.buf,
                &array_plan,
                target,
                self.frame_record,
                &mut slow,
                &mut zgc_fast_join,
            )
        } else {
            crate::runtime_lowering::emit_inline_tlab_newarray_fast_path_ir(
                &mut self.buf,
                &array_plan,
                target,
                self.frame_record,
                &mut slow,
            )
        };
        if !fast_ok {
            return false;
        }
        if false {
            eprintln!(
                "[cratonvm-jitc] ir newarray map sink: atype={element_type} bci={}",
                self.cur_bci
            );
        }
        if !zgc && self.gen_alloc_tail_admitted() {
            let map_start = self.buf.mark();
            let maps_before = self.oop_maps.len();
            self.emit_pending_call_clears();
            let sp_live_hi = self.spill_high_water;
            self.defer_frame_block_ensure = true;
            let map = self.emit_safepoint_map(sp_live_hi);
            self.defer_frame_block_ensure = false;
            let map_end = self.buf.pos();
            let slow_map_bytes = self.buf.as_slice()[map_start.len..map_end].to_vec();
            self.buf.rewind_to_mark(map_start);
            let ensure_frame_block = self
                .oop_maps
                .get(maps_before)
                .is_some_and(|m| !m.frame_slot_offsets.is_empty());
            let exit_bci = self.note_deferred_cross_call_exit();
            let slot = self.alloc_slot(id);
            self.deferred_gen_alloc_tails.push(DeferredGenAllocTail {
                map,
                slow,
                slow_map_bytes,
                ensure_frame_block,
                kind: DeferredGenAllocKind::Array {
                    target,
                    immediate,
                    length_slot,
                },
                resume: self.buf.pos(),
                exit_bci,
                node_pc: self.cur_node_pc,
                site_bci: self.cur_bci,
            });
            self.store_rax(slot);
            return true;
        }
        // The fast path's RAX is the bumped array (`MOV RAX, R11`; on ZGC the
        // announce helper's answer when the inline start-bit store refused).
        let join = self.emit_jmp_rel32();
        for patch in slow {
            self.patch_rel32_to_here(patch);
        }
        self.emit_pending_call_clears();
        let sp_live_hi = self.spill_high_water;
        let mut map = self.emit_safepoint_map(sp_live_hi);
        let slow_return = crate::runtime_lowering::emit_new_array_stub(
            &mut self.buf,
            self.context_slot_off,
            target,
            immediate,
            length_slot,
            self.frame_record,
        );
        self.anchor_safepoint_return(map.as_mut(), slow_return);
        self.note_inline_frame_return_site_at(slow_return);
        self.emit_shadow_reload();
        // As `Op::New`'s deferred arm: past the identity republish and the
        // null test only the stub owes, unless `CRATONVM_JIT_IR_ALLOC_FAST_JOIN=0`.
        // On ZGC the fast path may end in the announce helper, whose `0`
        // must reach the null test, as in the default arm: join before it.
        let fast_join = if !zgc {
            Some(join)
        } else {
            self.patch_rel32_to_here(join);
            None
        };
        self.emit_identity_after_shared_stub();
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
        self.buf.emit(&[0x0F, 0x85]); // JNZ allocated
        let allocated_patch = self.buf.pos();
        self.buf.emit(&[0; 4]);
        self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
        self.buf.emit_byte(0xE9); // JMP shared exception epilogue
        let exception_patch = self.buf.pos();
        self.buf.emit(&[0; 4]);
        self.push_call_exc_patch(exception_patch);
        let allocated = self.buf.pos();
        let rel = allocated as i32 - (allocated_patch as i32 + 4);
        Self::patch_or_bail(&mut self.buf, allocated_patch, rel);
        if let Some(join) = fast_join {
            self.patch_rel32_to_here(join);
        }
        if let Some(join) = zgc_fast_join {
            self.patch_rel32_to_here(join);
        }
        let slot = self.alloc_slot(id);
        self.store_rax(slot);
        true
    }

    /// gen r5w2/oomjit6 (`CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS`, default ON since gcf vf5): at the
    /// START of an armed `Op::Call` node, before any of its bytes, zero every
    /// word of the Java-argument staging region.
    ///
    /// The region sits in the frame's callee-saved band, which the band scan
    /// always reads, and nothing clears it: the arguments the LAST dispatched
    /// call staged there (the `WeakReference` constructor's referent, a
    /// `List.add`'s receiver) stay conservative roots until a later call stages
    /// over the same index -- and a direct call to a compiled callee stages
    /// nothing on its hot path (`CRATONVM_JIT_IR_COLD_ARG_STAGE`). Every writer
    /// of the region is a call lowering whose readers (the dispatch helper it
    /// calls, and that node's own cold paths) run inside the same node, so at a
    /// node boundary nothing staged there is read again before it is written.
    /// Each staged reference is also an input of its call, so it is in its home
    /// word while the call runs. The batched form uses XMM0, the lowerer's
    /// scratch register. Neither form changes a GPR or condition flags, so a
    /// carry into this node is undisturbed.
    pub(in crate::ir_lower) fn emit_dead_arg_stage_clears(&mut self) {
        let available = if self.args_stage_top_off > 0 {
            (self.args_stage_top_off as usize - 1) / 8 + 1
        } else {
            0
        };
        let count = self.args_stage_words.min(available);
        if self.operand_in_place_on && self.mir_mode == MirMode::Off && count >= 4 {
            self.buf.emit(&[0x66, 0x0F, 0xEF, 0xC0]); // PXOR XMM0, XMM0
            for i in 0..count / 2 {
                // All offsets are positive and no larger than the frame offset.
                let off = self.args_stage_top_off - (i as i32) * 16;
                self.emit_xmm_frame_move(XMM0, off, true);
            }
            if count % 2 != 0 {
                let off = self.args_stage_top_off - ((count - 1) as i32) * 8;
                self.emit_zero_frame_slot(off);
            }
        } else {
            for i in 0..count {
                let off = self.args_stage_top_off - (i as i32) * 8;
                self.emit_zero_frame_slot(off);
            }
        }
    }

    /// gen r5w5/oomjit9: fill [`Self::dead_home_clears`] -- see the section
    /// above [`ir_dead_home_clears_enabled`]. Runs once, before emission, off
    /// the slot plan and the schedule; leaves the map empty (zero nothing)
    /// when the dead-reference facts are absent or the union over the block
    /// graph does not converge. Returns whether the complete plan and clean
    /// entry premise also make dense home stores redundant. Argument staging
    /// retains the dense clear: its conservative primitive words are outside
    /// this reference-only stale-set proof.
    pub(in crate::ir_lower) fn plan_dead_home_clears(&mut self) -> bool {
        self.dead_home_clears.clear();
        self.loop_entry_dead_home_clears.clear();
        let schedule = self.schedule;
        let graph = self.graph;
        let slot_plan = self.slot_plan;
        let nb = schedule.blocks.len();
        let (words, ref_colours) = match self.dead_ref_clear.as_ref() {
            Some(plan) => (plan.words, plan.ref_colours.clone()),
            None => return false,
        };
        let mut ref_bits = vec![0u64; words];
        for &c in &ref_colours {
            bits_insert(&mut ref_bits, c as usize);
        }
        let stage_n = self.args_stage_words;
        let stage_words = stage_n.div_ceil(64).max(1);

        // 1. The dead colours at every armed site.
        let mut site_dead: HashMap<NodeId, Vec<u64>> = HashMap::new();
        for block in &schedule.blocks {
            for &nid in &block.nodes {
                let Some((pos, bci, site)) = self.dead_ref_clear_site_for(nid) else {
                    continue;
                };
                let dead = match self.dead_ref_clear.as_mut() {
                    Some(plan) => plan.dead_colours_at(pos, bci, site),
                    None => return false,
                };
                let mut bits = vec![0u64; words];
                for c in dead {
                    bits_insert(&mut bits, c as usize);
                }
                site_dead.insert(nid, bits);
            }
        }
        if site_dead.is_empty() {
            return false;
        }
        // JIT round 21, lane argwin (OOM21-3, opt-in): the reference
        // parameters' prologue homes dead at each site, merged into the
        // site's clears below (`plan_param_home_clears`).
        let param_clears = self.plan_param_home_clears(&site_dead);

        // gen r5w6/oomjit10: the clearable colours of each block's `Ref` phis
        // (non-empty only when the keep set is precise, which is what admits
        // phi colours into `ref_colours`). Their edge copies write them at
        // the END of every predecessor, which the walk below records as a
        // `Def` there.
        let mut phi_colours_at: Vec<Vec<usize>> = vec![Vec::new(); nb];
        {
            let block_of_merge: HashMap<NodeId, usize> = schedule
                .blocks
                .iter()
                .enumerate()
                .map(|(b, blk)| (blk.ctrl, b))
                .collect();
            for (id, node) in graph.nodes.iter().enumerate() {
                if !matches!(node.op, Op::Phi) || node.ty != IrType::Ref {
                    continue;
                }
                let Some(&b) = node.inputs.first().and_then(|m| block_of_merge.get(m)) else {
                    continue;
                };
                if let Some(Some(c)) = slot_plan.node_color.get(id).copied() {
                    if bits_contains(&ref_bits, c as usize) {
                        phi_colours_at[b].push(c as usize);
                    }
                }
            }
        }
        // 2. The block graph as the stale-set union sees it: per block, in
        //    order, every armed site, every `Ref` argument a call stages and
        //    every definition of a clearable colour.
        let mut blocks: Vec<StaleBlock> = schedule
            .blocks
            .iter()
            .map(|block| {
                let mut events = Vec::new();
                let in_body = block.nodes.len();
                for (k, nid) in block.nodes.iter().copied().chain(block.terminator).enumerate() {
                    let Some(node) = graph.nodes.get(nid as usize) else {
                        continue;
                    };
                    if let Some(dead) = site_dead.remove(&nid) {
                        // JIT round 20 wave 17, lane gcDead (I-9): an
                        // allocation site zeroes on its slow path only (or
                        // not at all), so it never cleans for what follows.
                        //
                        // JIT r24 (CF residual page, item 2;
                        // `CRATONVM_JIT_IR_OTHER_SITE_CLEANS`): nor does a
                        // prefixed call; every other armed site of the
                        // block's body -- a pinned non-call GC point such as
                        // `checkcast` or a helper `getstatic` -- zeroes its
                        // planned words at its first byte on every path (the
                        // block loop's planned-clear emission), so it cleans
                        // exactly as a plain call does. A terminator's
                        // planned words are never emitted: it never cleans.
                        let cleans = match node.op {
                            Op::Call { .. } => !self.call_site_may_take_inline_prefix(nid),
                            Op::New { .. } | Op::NewArray { .. } => false,
                            _ => k < in_body,
                        };
                        events.push(StaleEvent::Site {
                            node: nid,
                            dead,
                            cleans,
                        });
                    }
                    if matches!(node.op, Op::Call { .. }) {
                        let num_args = node.inputs.len().saturating_sub(2);
                        for i in 0..num_args.min(stage_n) {
                            let is_ref = node
                                .inputs
                                .get(2 + i)
                                .and_then(|&a| graph.nodes.get(a as usize))
                                .is_some_and(|a| a.ty == IrType::Ref);
                            if is_ref {
                                events.push(StaleEvent::Staged(i));
                            }
                        }
                    }
                    if let Some(Some(c)) = slot_plan.node_color.get(nid as usize).copied() {
                        if bits_contains(&ref_bits, c as usize) {
                            events.push(StaleEvent::Def(c as usize));
                        }
                    }
                }
                // gen r5w6/oomjit10: the edge copies into each successor's
                // phis, after everything else in this block.
                for &s in &block.successors {
                    if let Some(cols) = phi_colours_at.get(s) {
                        events.extend(cols.iter().map(|&c| StaleEvent::Def(c)));
                    }
                }
                StaleBlock {
                    events,
                    predecessors: block.predecessors.clone(),
                }
            })
            .collect();
        // The entry starts clean: the prologue (and the OSR stub, which enters
        // past it) zeroes every reference home it does not write
        // (`zero_ref_value_slots`); where that is switched off, a home's
        // pre-frame word is simply left alone, as before.
        // gci i5/ir (i4 open item O2): the block the method ENTERS, found by
        // its head (`scheduled_entry_block`); the `node_to_block[graph.entry]`
        // lookup was `usize::MAX` on every real schedule and only the `0`
        // fallback made it right.
        let entry_block = scheduled_entry_block(graph, schedule).unwrap_or(0);
        // gen r5w6/oomjit10: ...except the phis of a loop header (a block
        // entered from a later one), which an OSR entry stub seeds instead of
        // zeroing: they start stale, so a seeded value that dies is zeroed at
        // the first site that finds it dead.
        let mut seeded: Vec<usize> = Vec::new();
        for (b, block) in schedule.blocks.iter().enumerate() {
            if block.predecessors.iter().any(|&p| p >= b) {
                if let Some(cols) = phi_colours_at.get(b) {
                    seeded.extend_from_slice(cols);
                }
            }
        }
        if let Some(entry) = blocks.get_mut(entry_block) {
            let mut events: Vec<StaleEvent> = seeded.into_iter().map(StaleEvent::Def).collect();
            events.append(&mut entry.events);
            entry.events = events;
        }

        // 3. The forward union, and each site's stale sets at its first byte.
        let Some(at_sites) = stale_sets_at_sites(&blocks, entry_block, &ref_bits, stage_words)
        else {
            // JIT round 20 wave 17, lane gcDead (I-9): no plan, so no
            // allocation site is armed -- its claim would have no zeroing
            // behind it (see `disarm_alloc_site`).
            self.alloc_dead_home_clear = false;
            return false;
        };
        // JIT round 20 wave 17, lane gcDead: the zeroing the liveness claim
        // relies on is now in place at every armed site.
        self.dead_home_plan_converged = true;

        // 4. The words each site zeroes: the dead homes stale at it, and the
        //    stale staging words. `planned_slot_off`'s arithmetic for a home
        //    (inside the spill band only); a staging word only strictly above
        //    the spill band, so this can never write a colour's home through
        //    the staging arithmetic.
        let first_spill = self.first_spill;
        let spill_cap_off = self.spill_cap_off;
        let args_stage_top_off = self.args_stage_top_off;
        // gcd d5/u: the arithmetic below, so a census `off=` names its colour.
        if false {
            eprintln!(
                "[ir-slots] homes: colour c at [rbp - ({first_spill} + 8c)], spill cap {}, staging top {} x {}",
                spill_cap_off, args_stage_top_off, stage_n
            );
        }
        let home_off = |colour: u32| -> Option<i32> {
            let off = i32::try_from(u64::from(colour).saturating_mul(8))
                .ok()
                .and_then(|delta| first_spill.checked_add(delta))?;
            (off > 0 && off >= first_spill && off <= spill_cap_off).then_some(off)
        };
        let stage_off = |i: usize| -> Option<i32> {
            let off = i32::try_from(i)
                .ok()
                .and_then(|i| i.checked_mul(8))
                .and_then(|delta| args_stage_top_off.checked_sub(delta))?;
            (off > 0 && off > spill_cap_off).then_some(off)
        };
        let mut sites: HashMap<NodeId, Vec<i32>> = HashMap::new();
        for (nid, homes, stage) in at_sites {
            let mut offs: Vec<i32> = ref_colours
                .iter()
                .copied()
                .filter(|&c| bits_contains(&homes, c as usize))
                .filter_map(home_off)
                .collect();
            offs.extend(
                (0..stage_n)
                    .filter(|&i| bits_contains(&stage, i))
                    .filter_map(stage_off),
            );
            if !offs.is_empty() {
                offs.sort_unstable();
                offs.dedup();
                sites.insert(nid, offs);
            }
        }
        // OOM21-3 (lane argwin): a dead parameter's prologue home is zeroed
        // wherever the site zeroes (its first byte, or an allocation's slow
        // path). Below the spill band, so no staging rebase moves it.
        for (nid, homes) in param_clears {
            let offs = sites.entry(nid).or_default();
            offs.extend(homes);
            offs.sort_unstable();
            offs.dedup();
        }
        // 5. (`CRATONVM_JIT_IR_LOOP_ENTRY_DEAD_HOME_CLEARS`, default ON) a
        //    home an armed site INSIDE a loop zeroes, whose colour nothing in
        //    that loop writes, went stale before the loop: zero it once at
        //    the end of the loop's entry block instead of at every
        //    iteration's site ([`move_loop_entry_dead_home_clears`]).
        {
            let colour_of_off: HashMap<i32, usize> = ref_colours
                .iter()
                .filter_map(|&c| home_off(c).map(|off| (off, c as usize)))
                .collect();
            self.loop_entry_dead_home_clears = move_loop_entry_dead_home_clears(
                graph,
                schedule,
                &slot_plan.node_color,
                &blocks,
                &mut sites,
                &colour_of_off,
            );
            if !self.loop_entry_dead_home_clears.is_empty()
                && false
            {
                eprintln!(
                    "[ir-slots] loop-entry dead-home clears: {:?}",
                    self.loop_entry_dead_home_clears
                );
            }
        }
        if !sites.is_empty() && false {
            let mut listed: Vec<(&NodeId, &Vec<i32>)> = sites.iter().collect();
            listed.sort_unstable_by_key(|&(nid, _)| *nid);
            for (nid, offs) in listed {
                eprintln!("[ir-slots] dead-home clears at n{nid}: offsets {offs:?}");
            }
        }
        self.dead_home_clears = sites;
        // The stale union starts clean. Only replace the dense store form
        // when both normal and OSR entries establish that premise; otherwise
        // retain the old stores even though the sparse retention fix runs.
        self.reloc_emit_on
    }

    /// JIT round 21, lane argwin (OOM21-3, `CRATONVM_JIT_IR_PARAM_HOME_LIVENESS`,
    /// opt-in): per armed site of `site_dead` (its dead colour bits), the
    /// prologue homes `[rbp - 8(i+1)]` of the reference parameters dead there,
    /// to be zeroed with the site's clears. Empty off the switch, on an OSR
    /// compile, in a self-locking body (its exits release the method monitor
    /// from parameter 0's prologue home, [`Self::self_lock_obj_home`]), or
    /// when the entry block is not the first one laid out or has a
    /// predecessor (a loop back to the method's first instruction).
    ///
    /// A parameter's home is cleared at a site only when EVERY `Op::Param`
    /// node of that index is scheduled in the entry block before the site and
    /// has a colour the site's dead set holds (no value range covers the site,
    /// no frame state reachable after it names the colour: the keep set the
    /// site's own clears are built from, handler-aware local liveness
    /// included). After its node is emitted nothing reads the prologue home:
    /// the node's own coloured home holds the value, `frame_value_for` reads
    /// the prologue home only for a parameter whose node was never emitted,
    /// and the register fills happen in the prologue. A parameter with no node
    /// at all keeps its home (nothing proves who reads it). A site inside a
    /// splice clears no parameter home. The map keeps naming the home, which
    /// then holds null (or the dead-home poison).
    pub(in crate::ir_lower) fn plan_param_home_clears(
        &self,
        site_dead: &HashMap<NodeId, Vec<u64>>,
    ) -> HashMap<NodeId, Vec<i32>> {
        let mut out: HashMap<NodeId, Vec<i32>> = HashMap::new();
        if !ir_param_home_liveness_enabled()
            || self.graph.osr_compile
            || self.self_lock.is_some()
            || self.ref_param_homes.is_empty()
        {
            return out;
        }
        let graph = self.graph;
        let schedule = self.schedule;
        let plan = self.slot_plan;
        let nb = schedule.blocks.len();
        let Some(entry_block) = schedule
            .node_to_block
            .get(graph.entry as usize)
            .copied()
            .filter(|&b| b < nb)
        else {
            return out;
        };
        if entry_block != 0 || !schedule.blocks[entry_block].predecessors.is_empty() {
            return out;
        }
        // Prologue home -> every node of that reference parameter.
        let mut params: HashMap<i32, Vec<NodeId>> = HashMap::new();
        for (id, n) in graph.nodes.iter().enumerate() {
            let Op::Param(idx) = &n.op else {
                continue;
            };
            if n.ty != IrType::Ref {
                continue;
            }
            let off = (i32::from(*idx) + 1) * 8;
            let published = i16::try_from(off).is_ok_and(|o| self.ref_param_homes.contains(&o));
            let Ok(id) = NodeId::try_from(id) else {
                continue;
            };
            if published {
                params.entry(off).or_default().push(id);
            }
        }
        if params.is_empty() {
            return out;
        }
        let dbg = false;
        for (&site, bits) in site_dead {
            let Some(site_node) = graph.nodes.get(site as usize) else {
                continue;
            };
            if site_node
                .bytecode_pc
                .is_some_and(|pc| self.pc_is_in_a_spliced_body(pc))
            {
                continue;
            }
            let Some(pos) = plan.pos_of.get(site as usize).copied().flatten() else {
                continue;
            };
            for (&off, nodes) in &params {
                let dead_everywhere = nodes.iter().all(|&p| {
                    let in_entry =
                        schedule.node_to_block.get(p as usize).copied() == Some(entry_block);
                    let before = plan
                        .pos_of
                        .get(p as usize)
                        .copied()
                        .flatten()
                        .is_some_and(|pp| pp < pos);
                    let range_before = plan
                        .range
                        .get(p as usize)
                        .copied()
                        .flatten()
                        .is_some_and(|r| r.lo < pos && r.hi < pos);
                    let colour_dead = plan
                        .node_color
                        .get(p as usize)
                        .copied()
                        .flatten()
                        .is_some_and(|c| bits_contains(bits, c as usize));
                    in_entry && before && range_before && colour_dead
                });
                if dead_everywhere {
                    out.entry(site).or_default().push(off);
                    if dbg {
                        eprintln!("[ir-param-home] clear [rbp - {off}] at n{site} pos={pos}");
                    }
                }
            }
        }
        out
    }

    /// gen r5w2/oomjit6 (`CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS`, default ON since gcf vf5): at a
    /// GC point of an armed call site, `MOV qword [rbp - off], 0` for every
    /// reference home that is dead there (see the section above
    /// [`DeadRefSlotClear`]) and that the band scan would read at this
    /// safepoint (below `live_hi`). Writes no register and no flag, so it may
    /// sit anywhere a safepoint map may.
    pub(in crate::ir_lower) fn emit_dead_ref_slot_clears(&mut self, live_hi: i32) {
        // gen r5w3/live7: the facts may be built for the store-free claim
        // alone (`CRATONVM_JIT_PRECISE_FRAME_LIVENESS`), which writes nothing.
        // The incremental plan clears a dead colour at the first byte of
        // this call whenever any path may have written it since its previous
        // clear. Its def/phi-edge events cover every reference-home writer;
        // nothing in this call rewrites a dead home before this GC point.
        // With initialized entries and convergence, another dense clear here
        // can only store zero over zero. Retain it when that proof is absent.
        // gci i5/ir: an armed back-edge poll stores whatever the plan proved:
        // no planned first-byte clear runs before a poll.
        if !self.dead_ref_clear_stores
            || (self.dead_home_clears_complete && !self.poll_site_dense)
        {
            return;
        }
        let Some((pos, bci, site)) = self.dead_ref_clear_site else {
            return;
        };
        let dead = match self.dead_ref_clear.as_mut() {
            Some(plan) => plan.dead_colours_at(pos, bci, site),
            None => return,
        };
        let mut cleared: Vec<i32> = Vec::new();
        for colour in dead {
            let Some(off) = i32::try_from(u64::from(colour).saturating_mul(8))
                .ok()
                .and_then(|delta| self.first_spill.checked_add(delta))
            else {
                continue;
            };
            // Inside the spill band, and below the scan's cursor for this map:
            // a word above it is not BAND-scanned, so zeroing it buys nothing
            // -- unless the body publishes a frame block. The block names
            // every planned home for the whole activation, above the cursor
            // too (a home whose node comes later in linear order can have been
            // written earlier in time, e.g. by a loop body laid out after the
            // call), and the collector roots each named word through the
            // shadow stack whatever the cursor says (gen r5w3/live7 review).
            // Dead is dead on either side of the cursor: the rule above is
            // about live ranges and snapshots, not about the watermark.
            if off < self.first_spill
                || off > self.spill_cap_off
                || (off >= live_hi && self.frame_block.is_empty())
            {
                continue;
            }
            self.emit_dead_home_clear(off);
            cleared.push(off);
        }
        if !cleared.is_empty() && false {
            eprintln!("[ir-slots] dead-ref clear at pos={pos} bci={bci}: offsets {cleared:?}");
        }
    }

    /// gen r5w3/live7 (`CRATONVM_JIT_PRECISE_FRAME_LIVENESS`): the store-free
    /// form of [`Self::emit_dead_ref_slot_clears`] and
    /// [`Self::emit_dead_arg_stage_clears`]. At a GC point of an armed call
    /// site, returns `(covered, dead)`, both sorted and distinct, `dead` a
    /// subset of `covered`:
    ///
    ///   * **covered** -- the home word of every clearable reference colour
    ///     (`DeadRefSlotClear::ref_colours`: holds a `Ref`, never a phi) that
    ///     lies inside the spill band, and every word of the argument-staging
    ///     region;
    ///   * **dead** -- the homes of the colours `dead_colours_at` calls dead at
    ///     this call (no live range covers its position and no snapshot at a
    ///     bci reachable from it names the colour: the rule the store form
    ///     zeroes by, argued in the section above [`DeadRefSlotClear`]), and
    ///     the staging words at index `>= dead_ref_clear_site_args`. This call's
    ///     lowering never writes those (every staging store is `arg[i]` for
    ///     `i < num_args <= inputs.len() - 2`), so they hold an EARLIER node's
    ///     arguments, and the store form zeroes the whole region at this node's
    ///     first byte on the argument that nothing an earlier node staged is
    ///     read again. The words this call does stage stay covered-and-live:
    ///     one may be the argument's only home (`home_dropped`), and none is
    ///     rewritten by a relocation, so they must keep pinning.
    ///
    /// `None` -- no claim, nothing skipped -- off the flag, away from an armed
    /// site, or when nothing here is dead.
    pub(in crate::ir_lower) fn precise_liveness_claim(&mut self) -> Option<(Vec<i16>, Vec<i16>)> {
        if !self.frame_liveness_on {
            return None;
        }
        // JIT round 20 wave 17, lane gcDead (correctness, unconditional): a
        // home this claim drops is not relocated, and every LATER unarmed map
        // (a poll, a monitor enter, a helper-backed op) publishes every
        // defined node's home again. So a dropped home must be zero at the
        // collection, which only the converged dead-home plan guarantees --
        // not under `CRATONVM_JIT_IR_DEAD_HOME_CLEARS=0`, nor when its
        // stale-set union did not converge. No claim then (the old map).
        if !self.dead_home_plan_converged {
            return None;
        }
        let (pos, bci, site) = self.dead_ref_clear_site?;
        let first_spill = self.first_spill;
        let spill_cap_off = self.spill_cap_off;
        // `planned_slot_off`'s arithmetic, as `emit_dead_ref_slot_clears` does
        // it; an offset outside the spill band is simply not covered.
        let home_off = |colour: u32| -> Option<i16> {
            let off = i32::try_from(u64::from(colour).saturating_mul(8))
                .ok()
                .and_then(|delta| first_spill.checked_add(delta))?;
            if off <= 0 || off < first_spill || off > spill_cap_off {
                return None;
            }
            i16::try_from(off).ok()
        };
        let plan = self.dead_ref_clear.as_mut()?;
        let dead_colours = plan.dead_colours_at(pos, bci, site);
        let mut covered: Vec<i16> = plan
            .ref_colours
            .iter()
            .copied()
            .filter_map(home_off)
            .collect();
        let mut dead: Vec<i16> = dead_colours.into_iter().filter_map(home_off).collect();
        let (stage_covered, stage_dead) = staging_liveness_words(
            self.args_stage_top_off,
            self.args_stage_words,
            spill_cap_off,
            self.dead_ref_clear_site_args,
        );
        covered.extend(stage_covered);
        dead.extend(stage_dead);
        if dead.is_empty() {
            return None;
        }
        covered.sort_unstable();
        covered.dedup();
        dead.sort_unstable();
        dead.dedup();
        // `dead` is built from `covered`'s own sources, so this only guards a
        // future edit: a dead word nothing covers would be a claim the table
        // cannot represent, and the collector must not act on it.
        dead.retain(|off| covered.binary_search(off).is_ok());
        Some((covered, dead))
    }

    /// JIT round 25 item 2 (`CRATONVM_JIT_IR_SHADOW_LIVE_ACROSS_CALL`, default ON since R25-FLIP;
    /// the section above [`ir_shadow_live_across_call_enabled`]): at an armed
    /// GC point, the prologue words of the reference parameters whose copy
    /// has already run, which this safepoint's map and shadow push leave out
    /// -- `(dropped, zeroed)`, both ascending, `zeroed` the subset this call
    /// just stored zero to (`MOV qword [rbp - home], 0`; the rest the eager
    /// store of [`Self::zero_ref_param_home_after_copy`] zeroed already).
    ///
    /// A word is dropped only when all of these hold:
    ///
    ///   * its parameter is a [`ref_param_home_zero_candidates`] member (so
    ///     after the copy the word has no reader the copy does not also serve:
    ///     the section above [`ir_ref_param_home_zero_enabled`]);
    ///   * the copy is emitted, its store was not dropped, and its home is a
    ///     different word inside the spill band;
    ///   * the word is not the self-locking body's receiver;
    ///   * the copy runs before this site on every path
    ///     ([`param_copy_precedes_site`]: by schedule position and dominance,
    ///     never by emission order, so a deferred slow path's map placed after
    ///     the body is judged by its own site).
    ///
    /// Then the word is zero from here on whatever the collector does (the
    /// prologue is its only other writer), a zero word needs no relocation and
    /// no root, and the parameter's value stays published through its copy
    /// wherever the copy is live. Empty -- the old map byte for byte -- off
    /// the switch, in a frame-block body, or at a GC point that is not an
    /// armed site of a scheduled node.
    pub(in crate::ir_lower) fn shadow_live_param_home_drops(&mut self) -> (Vec<i16>, Vec<i16>) {
        let mut dropped: Vec<i16> = Vec::new();
        let mut zeroed: Vec<i16> = Vec::new();
        if self.shadow_live_param_homes.is_empty() || !self.frame_block.is_empty() {
            return (dropped, zeroed);
        }
        let Some((site_pos, _, site)) = self.dead_ref_clear_site else {
            return (dropped, zeroed);
        };
        let schedule = self.schedule;
        let nb = schedule.blocks.len();
        // A poll's synthetic site key (`poll_site_key`) is past the node
        // table, so it has no block here and drops nothing.
        let Some(site_block) = schedule
            .node_to_block
            .get(site as usize)
            .copied()
            .filter(|&b| b < nb)
        else {
            return (dropped, zeroed);
        };
        let self_lock_home = self.self_lock_obj_home();
        let pairs = self.shadow_live_param_homes.clone();
        for (id, home) in pairs {
            let home_off = i32::from(home);
            if home_off <= 0
                || home_off >= self.first_spill
                || !self.ref_param_homes.contains(&home)
                || self_lock_home == Some(home_off)
                || !self.defined_nodes.get(id as usize).copied().unwrap_or(false)
                || self.home_dropped.get(id as usize).copied().unwrap_or(false)
            {
                continue;
            }
            let Some(copy) = self
                .node_slot
                .get(id as usize)
                .copied()
                .flatten()
                .and_then(|off| i32::try_from(off.get()).ok())
            else {
                continue;
            };
            if copy <= 0 || copy == home_off || copy < self.first_spill {
                continue;
            }
            let Some(param_block) = schedule
                .node_to_block
                .get(id as usize)
                .copied()
                .filter(|&b| b < nb)
            else {
                continue;
            };
            let Some(param_pos) = self.slot_plan.pos_of.get(id as usize).copied().flatten() else {
                continue;
            };
            if !param_copy_precedes_site((param_block, param_pos), (site_block, site_pos), |a, b| {
                schedule.dom.dominates(a, b)
            }) {
                continue;
            }
            // The eager store ran right after the copy (it declines only on
            // the conditions refused above), so the word is zero already.
            if self.ref_param_home_zero.binary_search(&id).is_err() {
                // MOV qword [rbp - home], 0 -- written out rather than through
                // `emit_zero_frame_slot`, whose prologue forms do not apply at
                // a call site. No register, no flag.
                self.buf.emit(&[0x48, 0xC7, 0x85]);
                self.buf.emit(&(-home_off).to_le_bytes());
                self.buf.emit(&0i32.to_le_bytes());
                zeroed.push(home);
            }
            dropped.push(home);
        }
        dropped.sort_unstable();
        dropped.dedup();
        zeroed.sort_unstable();
        zeroed.dedup();
        (dropped, zeroed)
    }

    /// gen r5w3/live7: append safepoint `id`'s record to the body's liveness
    /// table. Every record of one body speaks for the SAME covered set (the
    /// clearable colours and the staging region are per-body facts); a record
    /// whose covered set differed would make the earlier records' `live`
    /// lists wrong for the union, so that poisons the table instead
    /// (`frame_liveness_poisoned`, read at finalize: no claim at all).
    pub(in crate::ir_lower) fn record_frame_liveness(&mut self, id: u32, covered: Vec<i16>, dead: &[i16]) {
        if self.frame_liveness_covered.is_empty() {
            self.frame_liveness_covered = covered;
        } else if self.frame_liveness_covered != covered {
            self.frame_liveness_poisoned = true;
            return;
        }
        let live: Vec<i16> = self
            .frame_liveness_covered
            .iter()
            .copied()
            .filter(|off| dead.binary_search(off).is_err())
            .collect();
        if false {
            eprintln!("[ir-slots] liveness claim at sp={id}: dead offsets {dead:?}");
        }
        self.frame_liveness_records
            .push(crate::SafepointFrameLiveness {
                sp_id: id,
                live,
                complete: true,
            });
    }

    /// gen r5w3/live7: the body's finished liveness table, or the empty table
    /// (no claim) when the flag was off, a record was poisoned, or any
    /// safepoint id store was deferred -- a deferred store leaves an older id
    /// in the frame word, and a claim keyed by that id would then describe a
    /// different program point.
    pub(in crate::ir_lower) fn take_frame_liveness(&mut self) -> crate::FrameLivenessTable {
        let covered = std::mem::take(&mut self.frame_liveness_covered);
        let mut records = std::mem::take(&mut self.frame_liveness_records);
        if !self.frame_liveness_on
            || self.frame_liveness_poisoned
            || self.self_call_id_stores_deferred > 0
            || covered.is_empty()
            || records.is_empty()
        {
            return crate::FrameLivenessTable::default();
        }
        // Ascending already (ids come from a counter); sorted rather than
        // asserted, as `sp_id_bcis` is, because `proves_dead` binary-searches.
        records.sort_by_key(|r| r.sp_id);
        crate::FrameLivenessTable { covered, records }
    }

    /// Emit the safepoint-id store and record the map for a GC-capable point.
    ///
    /// Call immediately BEFORE the call or allocation that can reach a
    /// collection, and before allocating the result slot of that node — the
    /// result slot is not written until the call returns, so publishing it as a
    /// live reference beforehand would hand the collector uninitialised memory.
    ///
    /// This ALWAYS stores an id and ALWAYS records a map, even when coverage
    /// cannot be established. Skipping either would leave the slot holding the
    /// id of an *earlier* safepoint, and the collector would then match a map
    /// describing a different program point and relocate against it. A map with
    /// `moving_young_coverage_complete: false` is the fail-closed answer: the
    /// verifier finds the entry, sees the flag, and diverts to the non-moving
    /// sweep for that cycle.
    pub(in crate::ir_lower) fn emit_safepoint_map(&mut self, live_hi: i32) -> Option<SafepointMapToken> {
        // gen r5w2/oomjit6 (opt-in): BEFORE the map and its shadow push, so a
        // cleared word is published (and copied back after the call) as the
        // null it now holds. Inert unless an armed site (an `Op::Call`, or under
        // `CRATONVM_JIT_IR_ALLOC_SITE_CLEARS` an allocation) armed it.
        self.emit_dead_ref_slot_clears(live_hi);
        if self.sp_id_slot_off <= 0 || !self.reloc_emit_on {
            return None;
        }
        // `live_ref_slots` returns an empty vector for two different reasons —
        // a genuinely oop-free safepoint, and a slot it could not describe.
        // Distinguish them, because the first is complete coverage and the
        // second is a refusal.
        let mut coverable = true;
        // Reference PARAMETER homes come first, because they are the one class
        // of live oop that is not a node result and so cannot be found by the
        // scan below. The prologue stores each incoming argument to
        // `[rbp - (idx+1)*8]` and that word keeps holding the reference for the
        // whole frame; `Op::Param`'s own spill slot is a COPY of it.
        //
        // Omitting them is not a partial claim, it is a false one: the band
        // scan is conservative, so a single unpublished young word anywhere in
        // the live band refuses the whole cycle. Measured on `IrEscapeProbe`
        // (`CRATONVM_MOVING_YOUNG_BAND_DBG`), the two rejected words were
        // `off=8 region=java-local` and `off=56 region=operand-spill` holding
        // the SAME reference — the parameter home and its node copy.
        let mut slots: Vec<i16> = self.ref_param_homes.clone();
        // Membership for the dedup below, one bit per frame byte offset
        // (round 11 wave 3, `r11-irlower-safepoint-map-compile-cost`): the
        // `Vec::contains` it replaces made every GC point O(R^2) in the number
        // of reference slots. An offset past the bitset (a frame that grew
        // after `frame_size` was read) falls back to the scan, so the published
        // list -- first occurrence, ascending node id -- is unchanged.
        let mut seen: Vec<u64> = std::mem::take(&mut self.sp_map_seen);
        seen.clear();
        seen.resize((usize::try_from(self.frame_size).unwrap_or(0) >> 6) + 2, 0);
        for &s in &slots {
            if let Ok(i) = usize::try_from(s) {
                if let Some(w) = seen.get_mut(i >> 6) {
                    *w |= 1u64 << (i & 63);
                }
            }
        }
        // Only the `Ref` nodes, in ascending id order (`ref_node_ids`): the
        // same slots in the same order as a walk over every node.
        for &id in &self.ref_node_ids {
            if !self.defined_nodes.get(id).copied().unwrap_or(false)
                || self.is_dropped_const(id as NodeId)
            {
                continue;
            }
            // `defined_nodes[id]` implies an allocated slot, so `None` here is
            // an internal inconsistency, not a `Ref` without a home. Treat it
            // exactly like an unencodable offset: fail closed, publish no map.
            let off = match self.node_slot.get(id).copied().flatten() {
                Some(off) => off.get() as i32,
                None => {
                    coverable = false;
                    break;
                }
            };
            if off > i16::MAX as i32 {
                coverable = false;
                break;
            }
            let off = off as i16;
            let idx = usize::try_from(off).unwrap_or(usize::MAX);
            let fresh = match seen.get_mut(idx >> 6) {
                Some(w) => {
                    let bit = 1u64 << (idx & 63);
                    let fresh = *w & bit == 0;
                    *w |= bit;
                    fresh
                }
                None => !slots.contains(&off),
            };
            if fresh {
                slots.push(off);
            }
        }
        self.sp_map_seen = seen;
        // gen r5w3/live7 (`CRATONVM_JIT_PRECISE_FRAME_LIVENESS`): a home this
        // call site proves dead is neither named by the map nor pushed to the
        // shadow stack, so the precise channels stop rooting it, and the
        // record below lets the band scan skip its word too. The frame block
        // is off under the flag (`plan_frame_block`): a block publishes every
        // home for the whole activation, dead or not -- unless
        // `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK_PRECISE` (r25), which relies on
        // the dead homes being zero here. Parameter homes are
        // never in `dead` (they are not colours).
        let liveness_claim = self.precise_liveness_claim();
        if let Some((_, dead)) = &liveness_claim {
            slots.retain(|s| dead.binary_search(s).is_err());
        }
        if !coverable {
            slots.clear();
        }
        // JIT round 25 item 2 (`CRATONVM_JIT_IR_SHADOW_LIVE_ACROSS_CALL`,
        // opt-in): a reference parameter's prologue word that duplicates its
        // copy is zeroed here (unless the eager store already did) and left out
        // of this map and of its push and reload. See
        // `shadow_live_param_home_drops`.
        let published_before = slots.len();
        let (param_home_drops, param_home_zeroed) = if coverable {
            self.shadow_live_param_home_drops()
        } else {
            (Vec::new(), Vec::new())
        };
        if !param_home_drops.is_empty() {
            slots.retain(|s| !param_home_drops.contains(s));
        }

        let id = self.next_sp_id;
        self.next_sp_id = self.next_sp_id.wrapping_add(1);
        let dbg_site = self
            .dead_ref_clear_site
            .filter(|_| self.shadow_live_dbg && !self.shadow_live_param_homes.is_empty());
        if let Some((_, _, site)) = dbg_site {
            eprintln!(
                "[ir-shadow-live] method={} sp={id} site=n{site} \
                 published_before={published_before} published_now={} \
                 dropped={param_home_drops:?} zeroed={param_home_zeroed:?}",
                self.graph.scope_method_key,
                slots.len(),
            );
        }
        // JIT round 19 proposal wave 8, lane gc8 (GC719-1): the frame words of
        // the `synchronized` blocks this safepoint holds, for the VM's
        // locked-monitor depths. Metadata only; nothing is emitted.
        if let Some(homes) = self.safepoint_monitor_homes(&slots) {
            self.ir_monitor_homes.push((id, homes));
        }
        // The id->bci translation a stack walk needs. `resume_bci` is the same
        // normalisation the throw-site and deopt paths apply: inside an
        // IR-spliced callee `cur_bci` is a pc in the COMBINED buffer, which
        // names no instruction in this method's own `Code`, so it is mapped
        // back to the enclosing invoke. `u32::try_from` cannot fail for a
        // spec-legal bci; a value that somehow exceeds it records nothing,
        // which reports no line rather than a wrong one.
        if let Ok(bci) = u32::try_from(self.resume_bci(self.cur_bci)) {
            self.sp_id_bcis.push((id, bci));
        }
        // Round 14 wave 2 (lane trace): the chain at THIS id, the innermost
        // frame's exact inline-chain key. `cur_node_pc`, never `cur_bci`, as
        // `note_inline_frame_return_site_at`: a pc-less node records an empty
        // chain (no frame rather than a wrong one).
        // Wave 58 (lane L2): a synchronized splice's enter names its callee.
        let chain_pc = self.sp_chain_pc_override.take().or(self.cur_node_pc);
        if !self.inline_frame_sites.is_empty() && crate::x64::inline_frame_map_enabled() {
            let chain: Vec<crate::x64::InlineFrameLevel> = match chain_pc {
                Some(node_pc) => self
                    .inline_frame_sites
                    .chain_at(node_pc)
                    .into_iter()
                    .map(|l| crate::x64::InlineFrameLevel {
                        label: l.method_key.into(),
                        bci: l.bci,
                        class_id: l.class_id,
                        // Wave 122 (L2): the optimizing tier stamps no bit.
                        hidden: None,
                    })
                    .collect(),
                None => Vec::new(),
            };
            self.inline_frame_sp_id_rows.push((id, chain));
        }
        // gen r5w3/live7: the claim is keyed by the id the frame word will
        // hold while this safepoint is active. A deferred id store leaves an
        // OLDER id there on the hot path, so such a site records nothing (and
        // the finalize drops the whole table if any store was deferred).
        if let Some((covered, dead)) = liveness_claim {
            if !self.defer_next_sp_id_store {
                self.record_frame_liveness(id, covered, &dead);
            }
        }
        // MOV qword [rbp - sp_id_slot_off], imm32 (sign-extended; ids are small)
        //
        // Round 11 wave 15 (lane fib): except for a direct self-call in a
        // reference-free body, whose id `emit_self_recursive_call` stores on
        // its cold paths instead (`deferred_self_call_sp_id`). The map and the
        // id->bci row above are recorded either way.
        if self.defer_next_sp_id_store {
            self.deferred_self_call_sp_id = Some(id);
            self.self_call_id_stores_deferred += 1;
        } else {
            self.emit_sp_id_store(id);
        }

        // Publish, and only then claim coverage. `emit_shadow_push` returns
        // false when the thread helper is unwired or there is nothing to
        // publish; an oop-free safepoint is still complete coverage, so the
        // claim below requires `coverable` AND (published OR nothing to
        // publish).
        //
        // With a frame block the whole block is published instead — once per
        // activation, as indirect entries the collector rewrites THROUGH — and
        // there is nothing to copy back after the call, so `pending_shadow`
        // stays empty and every reload / copy-back site below emits nothing.
        // The block names every word a map can name, because both come from
        // `planned_slot_off`; a map that named anything else would be a word
        // this frame publishes nowhere, so that refuses the body rather than
        // mixing the two protocols (they share the saved-base word).
        let block_mode = !self.frame_block.is_empty();
        if block_mode
            && !slots
                .iter()
                .all(|s| self.frame_block.binary_search(s).is_ok())
        {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "ir_lower: a safepoint map names a reference word outside the frame block",
            )));
        }
        let published = if !block_mode {
            self.emit_shadow_push(&slots)
        } else if slots.is_empty() {
            false
        } else if self.defer_frame_block_ensure {
            true
        } else {
            self.emit_frame_block_ensure()
        };
        if self.dbg_ir_reloc_on {
            eprintln!(
                "[ir-reloc] safepoint id={id} slots={slots:?} coverable={coverable} \
                 published={published} frame_block={:?} thread_helper={:#x} thread_slot={} \
                 savebase={} ss_off={}",
                self.frame_block,
                self.get_current_thread,
                self.shadow_thread_slot_off,
                self.shadow_savebase_slot_off,
                self.shadow_off_in_thread,
            );
        }
        self.shadow_pushed_any |= published;
        self.pending_shadow = if published && !block_mode {
            slots.clone()
        } else {
            Vec::new()
        };
        let complete = coverable && (published || slots.is_empty());
        // §25.3's diagnostic count, the same quantity the single-pass backend
        // records (`x64/safepoint.rs`, `shadow_pushed_here`): how many slots
        // THIS safepoint's shadow push published. Diagnostic only — read by
        // `conservative_roots`' band report, never by a collector decision.
        let shadow_pushed = if published {
            u16::try_from(slots.len()).unwrap_or(u16::MAX)
        } else {
            0
        };

        let index = self.oop_maps.len();
        self.oop_maps.push(crate::OopMapEntry {
            // An emitter that owns a native call can bind its actual return
            // address after the instruction has been emitted.
            native_pc_offset: 0,
            bytecode_pc: id,
            frame_slot_offsets: slots,
            // The claim the collector acts on: the shadow stack HAS been
            // published with every live oop of this frame, so each can be
            // rewritten in place. Requires both halves — `coverable` (every
            // live `Ref` slot could be named) and `published` (the push was
            // actually emitted). Naming the slots alone is not enough:
            // `scan_oop_slots` yields `ObjectRef` values, which mark but cannot
            // be written back through.
            moving_young_coverage_complete: complete,
            // Precise GPR oop mask over ALL_SPILL_GPRS positions:
            // when reference residency is active, record the assigned resident GPRs;
            // otherwise claim no registers hold oops (Some(0)).
            reg_oop_mask: if self.ref_residency_on {
                let mask: u16 = (0..16).fold(0u16, |m, reg| {
                    if let Some(owner) = self.gp_reg_owner[reg] {
                        if self.graph.nodes.get(owner as usize).map(|n| n.ty) == Some(IrType::Ref) {
                            m | crate::x64::reg_oop_mask_bit(reg as u8)
                        } else {
                            m
                        }
                    } else {
                        m
                    }
                });
                Some(mask)
            } else {
                Some(0)
            },
            live_frame_hi: live_hi,
            // The IR tier allocates frame slots; it does not home local `k` at
            // `[rbp - 8*(k+1)]`, so the locals-band oracle does not apply to
            // these frames and must not answer as if it did.
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            // The IR tier has no java-locals band to speak for, but its slot
            // COLOURING is a proof about every spill slot it allocated: a
            // non-`Ref` colour never holds an object pointer, because
            // `assign_colors` never moves a colour between its two free lists.
            // Exact because `verify_slot_colouring` re-derives the no-aliasing
            // property rather than trusting it.
            //
            // The premise is `IrType::Ref`, which is the SAME premise this
            // function publishes roots on a few lines above. So the oracle
            // cannot be wrong here unless the map itself already is: a
            // non-`Ref` node holding a real object pointer would be an
            // unpublished root today, with or without this field. It is not an
            // independent check of that, and must not be read as one.
            non_oop_stack_slots: self.prim_slot_offsets.clone(),
            stack_marks_exact: true,
            // Round 9: this said "The IR tier emits no shadow push of its
            // own" and recorded 0, but `emit_shadow_push` above IS this tier's
            // push, so every IR map read as "pushed nothing" in the band report.
            shadow_pushed,
            // gcg g3/v: no single-pass blind image on this tier.
            spill_written: None,
        });
        Some(SafepointMapToken {
            index,
            id,
            id_stored: !self.defer_next_sp_id_store,
            last_return: 0,
        })
    }

    pub(in crate::ir_lower) fn emit_deferred_safepoint_id_store(
        &mut self,
        id: Option<u32>,
        map: &mut Option<SafepointMapToken>,
    ) {
        let Some(id) = id else {
            return;
        };
        if map.as_ref().is_some_and(|map| map.id != id) {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "deferred IR safepoint ID disagrees with its map",
            )));
            return;
        }
        self.emit_sp_id_store(id);
        if let Some(map) = map {
            map.id_stored = true;
        }
    }

    /// Bind an emitted call's exact return to the map active at that call.
    /// A shared stub can emit frame republication after the call, so its
    /// returned offset can precede the current buffer position.
    pub(in crate::ir_lower) fn anchor_safepoint_return(
        &mut self,
        token: Option<&mut SafepointMapToken>,
        return_address: usize,
    ) {
        if token.as_deref().is_some_and(|token| !token.id_stored) {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "IR helper return has no published safepoint ID",
            )));
            return;
        }
        self.record_safepoint_return(token, return_address);
    }

    pub(in crate::ir_lower) fn anchor_recursive_caller_return(
        &mut self,
        token: Option<&mut SafepointMapToken>,
        return_address: usize,
    ) {
        if let Some(token) = token.as_deref().filter(|token| !token.id_stored) {
            // The root-free caller is placed by its return PC, not its old ID.
            let root_free = self.body_publishes_no_roots()
                && self.oop_maps.get(token.index).is_some_and(|map| {
                    map.frame_slot_offsets.is_empty()
                        && map.moving_young_coverage_complete
                        && map.shadow_pushed == 0
                });
            if !root_free {
                self.latch_bailout(Bailout::new(BailoutReason::Internal(
                    "unpublished IR caller map is not reference-free",
                )));
                return;
            }
        }
        self.record_safepoint_return(token, return_address);
    }

    pub(in crate::ir_lower) fn record_safepoint_return(
        &mut self,
        token: Option<&mut SafepointMapToken>,
        return_address: usize,
    ) {
        let Some(token) = token else {
            return;
        };
        let offset = u32::try_from(return_address).ok().filter(|&pc| {
            pc > token.last_return && return_address <= self.buf.pos()
        });
        let map = self.oop_maps.get_mut(token.index).filter(|map| map.bytecode_pc == token.id);
        match (map, offset) {
            (Some(map), Some(offset)) => {
                token.last_return = offset;
                if map.native_pc_offset == 0 {
                    map.native_pc_offset = offset;
                } else {
                    // Alternate native returns share one logical map ID
                    // and identical frame coverage.
                    let mut alias = map.clone();
                    alias.native_pc_offset = offset;
                    self.oop_maps.push(alias);
                }
            }
            _ => self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "invalid IR safepoint return anchor",
            ))),
        }
    }

    // ── Phi resolution (BUG FIX [jit-irlower #2]) ────────────────────────
    //
    // Previously `Op::Phi` only called `alloc_slot` with the comment
    // "predecessors will write it" — but no code ever emitted those writes,
    // so a phi read an uninitialised frame slot. We now perform a standard
    // edge-split parallel copy: before a predecessor block branches to a
    // merge/region successor, store each incoming phi-argument value into
    // the corresponding phi's slot.

    /// Reserve a frame slot for every `Op::Phi` up front.
    ///
    /// A forward branch can target a merge block whose phis have not yet
    /// been lowered, so the destination slots must exist before any edge
    /// copy is emitted. Phis are skipped by `lower_data_node`.
    ///
    /// Fallible: this is the first thing that can exhaust the frame, and a
    /// refusal here must reach `lower_inner` as a `FrameTooLarge` bailout
    /// rather than a panic.
    pub(in crate::ir_lower) fn prealloc_phi_slots(&mut self) -> CompileResult<()> {
        for id in 0..self.graph.nodes.len() {
            if matches!(self.graph.nodes[id].op, Op::Phi) {
                self.alloc_slot_checked(id as NodeId)?;
            }
        }
        Ok(())
    }

    /// Resolve the block that produces control token `ctrl` by walking up
    /// control inputs until we reach a node that heads some block.
    ///
    /// Merge predecessor `k` records `self.ctrl` (the predecessor's live
    /// control node) as `merge.inputs[k]`; that token is either a block
    /// head directly (goto fall-through) or a `Proj` off an `If` (block
    /// head). Walking control inputs makes the mapping robust to either.
    ///
    /// Delegates to the free function [`ctrl_block_of`] so the slot planner,
    /// which must reproduce this exact edge attribution to see phi inputs as
    /// uses at the *predecessor's* terminator, cannot drift from the emitter.
    pub(in crate::ir_lower) fn block_of_ctrl(&self, ctrl: NodeId) -> Option<usize> {
        ctrl_block_of(self.graph, self.schedule, ctrl)
    }

    /// Emit the parallel-copy stores for every phi at `succ_block` whose
    /// merge has `pred_block` as the source for that phi argument.
    ///
    /// Each copy loads the incoming SSA value (already spilled by the
    /// predecessor block, which is lowered before its terminator) into RAX
    /// and stores it into the phi's reserved slot.
    ///
    /// The copies are a PARALLEL assignment: every source is read as it was
    /// *before* any destination is written. They are therefore sequentialised
    /// by [`resolve_parallel_copy`] rather than emitted in gather order, and a
    /// cycle among them is broken through [`Lowerer::phi_copy_scratch_slot_off`].
    /// See the doc comment on the emission loop below for what was wrong before.
    ///
    /// Round 9 wave 6: every call site of this function is straight-line code
    /// of ONE edge, after the edge's back-edge poll, so it is also where the
    /// split plan's edge-resolution copies run -- after the phi copies, see
    /// [`Lowerer::ls_emit_edge_copies`].
    pub(in crate::ir_lower) fn emit_phi_copies(&mut self, pred_block: usize, succ_block: usize) {
        self.emit_value_phi_copies(pred_block, succ_block);
        self.ls_emit_edge_copies(pred_block, succ_block);
    }

    /// The split plan's CFG-edge resolution for `pred_block -> succ_block`
    /// (round 9 wave 6; [`LsEdgeCopies`] states the design). Nothing unless
    /// `CRATONVM_JIT_IR_LS_EDGE_RESOLUTION` admitted a value with a copy here.
    ///
    /// # Why AFTER the phi copies, and why every copy is a home load
    ///
    /// The phi copies read their sources from registers and publish their phis
    /// into registers. Running the resolution first could overwrite a register
    /// a phi source is still to be read from; running it after can only be
    /// wrong if it READS something the phi copies wrote (it reads home words
    /// only, and a phi copy writes phi home words, the phi-copy scratch and
    /// registers) or WRITES a register a phi was just published into. Both are
    /// checked here per copy, against this edge's own phi copies, and either
    /// refuses the compile -- the planner's `held_by_other` test and the
    /// slot colouring both say neither can happen, so a hit is a model that has
    /// come apart, not a shape to emit.
    ///
    /// # Why the residency tables are NOT updated
    ///
    /// The code of `succ_block` was, or will be, emitted against the state the
    /// emitter carries out of `succ_block`'s LAYOUT predecessor, and these
    /// copies exist to make the machine match that state on this edge. The
    /// emitter's own tables after this point describe the next block in layout
    /// order, which this edge does not reach (the edge is never a fall-through:
    /// `split_edge_resolution` skips those), so publishing here would be
    /// telling the next block something that is not true on ITS paths.
    pub(in crate::ir_lower) fn ls_emit_edge_copies(&mut self, pred_block: usize, succ_block: usize) {
        if self.ls_edge_copies.is_empty() {
            return;
        }
        let Some(idx) = self
            .ls_edge_copies
            .iter()
            .position(|e| e.pred == pred_block && e.succ == succ_block)
        else {
            return;
        };
        let copies = self.ls_edge_copies[idx].copies.clone();
        let phis = self.gather_phi_copies(pred_block, succ_block);
        for &(node, reg, gp) in &copies {
            let off = self.slot_of(node);
            let phi_reg_clash = phis.iter().any(|c| {
                if gp {
                    self.assigned_gpr(c.phi) == Some(reg)
                } else {
                    self.assigned_xmm(c.phi) == Some(reg)
                }
            });
            let home_clash =
                off == self.phi_copy_scratch_slot_off || phis.iter().any(|c| c.dst == off);
            if off <= 0 || phi_reg_clash || home_clash {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::Internal(
                        "ir_lower: an edge-resolution copy collides with the edge's phi copies",
                    ),
                    format!(
                        "n{node} -> {} {reg} on block {pred_block} -> {succ_block} \
                         (home [rbp-{off}], phi register clash {phi_reg_clash}, \
                         home clash {home_clash})",
                        if gp { "gp" } else { "xmm" }
                    ),
                ));
                return;
            }
            if gp {
                self.load_reg_from_frame(reg, off);
            } else {
                let is_double = self
                    .graph
                    .nodes
                    .get(node as usize)
                    .is_some_and(|n| matches!(n.ty, IrType::Double));
                self.fp_load(reg, off, is_double);
            }
            self.ls_edge_loads += 1;
        }
        if let Some(cell) = self.ls_edge_copies_done.get_mut(idx) {
            *cell = true;
        }
    }

}
