#[allow(unused_imports)]
use super::*;

impl<'a> Lowerer<'a> {
    pub(in crate::ir_lower) fn lower_data_node(&mut self, id: NodeId) {
        // W7-C: a sink decision belongs to the one `Op::Call` that made it.
        self.tail_sink.cur = false;
        // real-frame-deopt: anchor the node's bytecode pc to the earliest
        // native offset emitted for it, so safepoint snapshots can be keyed
        // by native offset. `self.graph` is a `&'a Graph`, so reading
        // `bytecode_pc` here does not borrow `self`.
        self.cur_node_pc = self.graph.nodes[id as usize].bytecode_pc;
        self.note_row_census_node();
        if let Some(pc) = self.graph.nodes[id as usize].bytecode_pc {
            // j27 wave 2 (lane GCMAP; `j27-mhfilter-pre-call-frame-bci-is-the-getstatic-20261028.md`):
            // `cur_bci` is the bci a frame parked here REPORTS (safepoint-id
            // and return-site tables, the throw stamp); a constant filter
            // species' PRE call reports its `invokeExact`'s. The by-pc rows and
            // the deopt anchor below stay keyed by the node's own `pc`.
            self.cur_bci = self.graph.reported_call_bci(id, pc);
            self.note_bci_anchor(id, pc);
        }
        // Per-copy deopt metadata: a node belonging to a specific copy of a
        // cloned body anchors its OWN snapshot, at an offset inside that copy.
        // `None` (every node of every graph that did not unroll) leaves both
        // the map and every frame resolution exactly as they were.
        self.cur_node_frame = self.graph.nodes[id as usize].frame_snapshot;
        if let Some(si) = self.cur_node_frame {
            let here = self.buf.pos();
            self.snapshot_native
                .entry(si)
                .and_modify(|e| {
                    if here < *e {
                        *e = here;
                    }
                })
                .or_insert(here);
        }

        // The branch/guard TEST owns these ANDs. The opcode arm still
        // reserves the snapshot home when a deopt pad must rebuild one.
        if self.reg_alloc_on
            && !self
                .fused_test_and
                .get(id as usize)
                .copied()
                .unwrap_or(false)
            && !self
                .test_and_remat
                .get(id as usize)
                .copied()
                .unwrap_or(false)
            && self
                .mir
                .as_ref()
                .and_then(|plan| plan.allocated_instruction(id))
                .is_some()
        {
            let slot = self.alloc_slot(id);
            if self.reg_alloc_direct_alu(id, slot) {
                self.mir_tiles += 1;
                return;
            }
            if self.value_flow.requires_selected_emission(id) {
                // Its graph operands include definitions this attempt omitted.
                self.latch_bailout(Bailout::new(BailoutReason::UnsupportedShape(
                    "folded integer instruction could not bind allocated operands",
                )));
                return;
            }
        }
        let node = &self.graph.nodes[id as usize];
        match &node.op {
            Op::Const(val) => {
                let val = *val;
                let slot = self.alloc_slot(id);
                // CG16P-4: a dropped constant home is never read.
                if self.is_dropped_const(id) {
                    self.home_stores_dropped += 1;
                } else if !self.emit_store_frame_imm32(slot, val) {
                    self.emit_mov_rax_imm64(val);
                    self.store_rax(slot);
                }
            }
            Op::Param(idx) => {
                // Params were stored to local frame slots by the prologue.
                // local_offset(i) = (i + 1) * 8
                let param_offset = ((*idx as i32) + 1) * 8;
                // Round 11 wave 14 (lane fib, proposal F13-2): an INT or LONG
                // parameter's home IS its prologue word, which the prologue
                // wrote and nothing rewrites, so there is no copy to store --
                // only the register to fill. See `alias_param_home`.
                if matches!(node.ty, IrType::Int | IrType::Long)
                    && self.alias_param_home(id, param_offset)
                {
                    if self.poisons_int_def(id) {
                        // The poisoning stress mode, in place: the prologue
                        // word IS the home. `MOV DWORD [home + 4], imm32`.
                        self.buf.emit_byte(0xC7);
                        self.emit_rbp_modrm_disp(0, param_offset - 4);
                        // Cast: the poison's high half, exactly.
                        self.buf.emit(&((INT_POISON_HIGH >> 32) as u32).to_le_bytes());
                    }
                    // Round 14 wave 7 (lane callcost3, CC6-2): the prologue
                    // already filled the register from the ABI register, and
                    // nothing has been published into it since.
                    if self.param_register_filled(id) {
                        return;
                    }
                    if self.param_register_fills.iter().any(|&(n, _)| n == id)
                        && false
                    {
                        eprintln!(
                            "[ir-param-fill] n{id}: its register was published over; reloaded"
                        );
                    }
                    self.publish_gp_from_slot(id, param_offset);
                    return;
                }
                let slot = self.alloc_slot(id);
                self.load_to_rax(param_offset);
                // An FP parameter is a loop invariant often enough to be worth
                // a register; the copy comes from the home word this just
                // wrote, because the prologue delivered it through a GPR.
                //
                // An INT or LONG parameter is exactly as much a loop invariant,
                // and got nothing until 2026-09-03. `gp_store_value` writes the
                // home word and then copies RAX into the register, so the
                // publish here is register-to-register — no reload of a word
                // this arm just wrote.
                if matches!(node.ty, IrType::Int | IrType::Long) {
                    self.gp_store_value(id, slot, RAX);
                } else {
                    self.store_rax(slot);
                    if matches!(node.ty, IrType::Float | IrType::Double) {
                        self.publish_fp_from_slot(id, slot, node.ty == IrType::Double);
                    }
                }
                // gci i3/ir (opt-in `CRATONVM_JIT_IR_REF_PARAM_HOME_ZERO`): the
                // copy is now the parameter's only home; its prologue word
                // stops being a second, never-cleared one.
                if node.ty == IrType::Ref {
                    self.zero_ref_param_home_after_copy(id, slot, param_offset);
                }
            }
            Op::Add => {
                let slot = self.alloc_slot(id);
                if self.test_and_remat.get(id as usize).copied().unwrap_or(false) {
                    return;
                }
                if matches!(node.ty, IrType::Float | IrType::Double) {
                    // ADDSS/ADDSD XMM0, XMM1. JIT r22 wave 6b
                    // (`CRATONVM_JIT_FP_NAN_OPERAND_ORDER`, opt-in since wave 7): value2
                    // in the destination, HotSpot's interpreter order, so two NaN operands
                    // answer value2's NaN; the sum's bits are the same for
                    // every non-NaN pair.
                    let is_d = node.ty == IrType::Double;
                    let (dst_in, src_in) = if crate::fp_nan_operand_order_enabled() {
                        (node.inputs[1], node.inputs[0])
                    } else {
                        (node.inputs[0], node.inputs[1])
                    };
                    self.fp_binop_value(id, slot, 0x58, dst_in, src_in, is_d);
                } else {
                    if self.try_emit_fused_add_i2l(id, slot) {
                        return;
                    }
                    let form = self.emit_add_lea(id, slot, false, node.ty != IrType::Int);
                    if matches!(form, AddForm::No) {
                        // W9-2 (`CRATONVM_JIT_IR_REG_ALLOC`): straight into its register.
                        if self.reg_alloc_direct_alu(id, slot) {
                            return;
                        }
                        self.gp_load_value(RAX, node.inputs[0]);
                        if !self.emit_alu_acc_imm(node.inputs[1], 0x05, node.ty != IrType::Int) {
                            // Round 17: or read in place from its register (`emit_alu_acc_reg`).
                            if !self.emit_alu_acc_reg(node.inputs[1], 0x05, node.ty != IrType::Int)
                                // JIT rx27 wave 8: or from its home word in place.
                                && !self.emit_alu_acc_frame(
                                    node.inputs[1],
                                    0x05,
                                    node.ty != IrType::Int,
                                )
                            {
                                self.gp_load_value(RCX, node.inputs[1]);
                                if node.ty == IrType::Int {
                                    // ADD EAX, ECX
                                    self.buf.emit(&[0x01, 0xC8]);
                                } else {
                                    // ADD RAX, RCX
                                    self.buf.emit(&[0x48, 0x01, 0xC8]);
                                }
                                // Round 9 wave 11 (irl11): `ADD RCX, RAX` if the
                                // sum turns out to be an RCX carry (`RcxTwin`).
                                self.offer_alu_rcx_twin();
                            }
                        }
                    }
                    // `Done` settled the home inside the helper, having
                    // published the register itself; the other two leave the
                    // value in RAX for this one store, exactly as the single
                    // path used to.
                    if !matches!(form, AddForm::Done) {
                        self.store_rax(slot);
                    }
                }
            }
            Op::Sub => {
                let slot = self.alloc_slot(id);
                if matches!(node.ty, IrType::Float | IrType::Double) {
                    // SUBSS/SUBSD XMM0, XMM1
                    let is_d = node.ty == IrType::Double;
                    self.fp_binop_value(id, slot, 0x5C, node.inputs[0], node.inputs[1], is_d);
                } else {
                    let form = self.emit_add_lea(id, slot, true, node.ty != IrType::Int);
                    if matches!(form, AddForm::No) {
                        // W9-2 (`CRATONVM_JIT_IR_REG_ALLOC`): straight into its register.
                        if self.reg_alloc_direct_alu(id, slot) {
                            return;
                        }
                        self.gp_load_value(RAX, node.inputs[0]);
                        if !self.emit_alu_acc_imm(node.inputs[1], 0x2D, node.ty != IrType::Int) {
                            // Round 17: or read in place from its register (`emit_alu_acc_reg`).
                            if !self.emit_alu_acc_reg(node.inputs[1], 0x2D, node.ty != IrType::Int)
                                // JIT rx27 wave 8: or from its home word in place.
                                && !self.emit_alu_acc_frame(
                                    node.inputs[1],
                                    0x2D,
                                    node.ty != IrType::Int,
                                )
                            {
                                self.gp_load_value(RCX, node.inputs[1]);
                                if node.ty == IrType::Int {
                                    // SUB EAX, ECX
                                    self.buf.emit(&[0x29, 0xC8]);
                                } else {
                                    // SUB RAX, RCX
                                    self.buf.emit(&[0x48, 0x29, 0xC8]);
                                }
                            }
                        }
                    }
                    if !matches!(form, AddForm::Done) {
                        self.store_rax(slot);
                    }
                }
            }
            Op::Mul => {
                let slot = self.alloc_slot(id);
                if matches!(node.ty, IrType::Float | IrType::Double) {
                    // MULSS/MULSD XMM0, XMM1, value2 in the destination
                    // under `CRATONVM_JIT_FP_NAN_OPERAND_ORDER` (see `Op::Add`).
                    let is_d = node.ty == IrType::Double;
                    let (dst_in, src_in) = if crate::fp_nan_operand_order_enabled() {
                        (node.inputs[1], node.inputs[0])
                    } else {
                        (node.inputs[0], node.inputs[1])
                    };
                    self.fp_binop_value(id, slot, 0x59, dst_in, src_in, is_d);
                } else {
                    // Round 9 wave 10 (irl10): `IMUL RAX, r, imm` straight
                    // from a resident first operand, instead of `MOV RAX, r;
                    // IMUL RAX, RAX, imm`. Declines (emitting nothing) unless
                    // the second operand folds and the first is resident and
                    // not carried, so the accumulator sequence below is
                    // unchanged whenever it does not fire.
                    let wide = node.ty != IrType::Int;
                    // W9-2 (`CRATONVM_JIT_IR_REG_ALLOC`): straight into its register.
                    if self.reg_alloc_direct_alu(id, slot) {
                        return;
                    }
                    // JIT r23 wave 11 (lane L7): or `k * x`, constant first.
                    if !self.emit_imul_const_first(node.inputs[0], node.inputs[1], wide)
                        && !self.emit_imul_reg_imm3(node.inputs[0], node.inputs[1], wide)
                    {
                        self.gp_load_value(RAX, node.inputs[0]);
                        if !self.emit_imul_imm(node.inputs[1], node.ty != IrType::Int) {
                            self.gp_load_value(RCX, node.inputs[1]);
                            if node.ty == IrType::Int {
                                // IMUL EAX, ECX
                                self.buf.emit(&[0x0F, 0xAF, 0xC1]);
                            } else {
                                // IMUL RAX, RCX
                                self.buf.emit(&[0x48, 0x0F, 0xAF, 0xC1]);
                            }
                            // `IMUL RCX, RAX` for an RCX carry (`RcxTwin`).
                            self.offer_alu_rcx_twin();
                        }
                    }
                    self.store_rax(slot);
                }
            }
            Op::Div => {
                let slot = self.alloc_slot(id);
                // FP division has NO zero/overflow guard: IEEE x/0 is ±inf/NaN,
                // never an exception, so an `fdiv`/`ddiv` is a plain DIVSS/DIVSD
                // and is never a deopt point.
                if matches!(node.ty, IrType::Float | IrType::Double) {
                    let is_d = node.ty == IrType::Double;
                    self.fp_binop_value(id, slot, 0x5E, node.inputs[0], node.inputs[1], is_d);
                    return;
                }
                self.gp_load_value(RAX, node.inputs[0]);
                if let Some(d) = self.reducible_const_divisor(node.ty, node.inputs[1]) {
                    self.emit_div_by_const(node.ty == IrType::Long, d, false);
                } else {
                    self.emit_integer_div_rem(id, node, false);
                }
                self.store_rax(slot);
            }
            Op::Rem => {
                let slot = self.alloc_slot(id);
                // FP remainder (`frem`/`drem`) is `fmod`-style with no single SSE
                // instruction, so it is lowered as a CALL to the jit_frem/jit_drem
                // runtime helper. The float ABI passes the two args in XMM0/XMM1
                // and returns in XMM0 on both Win64 and SysV — which is exactly
                // the IR's own XMM scratch convention — so no register shuffling
                // is needed: load the operands, MOV RAX,helper ; CALL RAX, store
                // the XMM0 result. The 32-byte Win64 shadow space and 16-byte
                // call alignment are reserved unconditionally by the frame layout
                // (see `Lowerer::new`), so this CALL is safe even in an otherwise
                // call-free method. The helper never throws/deopts (IEEE `fmod`
                // has no exceptional result — `x % 0.0` is NaN, not a trap), so
                // there is NO exception-sentinel check, unlike `Op::Call`.
                if matches!(node.ty, IrType::Float | IrType::Double) {
                    let is_d = node.ty == IrType::Double;
                    self.fp_load_value(XMM0, node.inputs[0], is_d); // a
                    self.fp_load_value(XMM1, node.inputs[1], is_d); // b
                    let helper = if is_d { self.drem } else { self.frem };
                    self.emit_mov_reg_imm64(RAX, helper as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    self.fp_store_value(id, slot, XMM0, is_d);
                    return;
                }
                self.gp_load_value(RAX, node.inputs[0]);
                if let Some(d) = self.reducible_const_divisor(node.ty, node.inputs[1]) {
                    self.emit_div_by_const(node.ty == IrType::Long, d, true);
                } else {
                    self.emit_integer_div_rem(id, node, true);
                }
                self.store_rax(slot);
            }
            // Round 11 wave 13 (lane `cha`): the exact receiver-class TEST the
            // `Op::Guard` before a CHA-bound caller-held synchronized CALL's
            // `MonitorEnter` consumes (`IrBuilder::enter_receiver_monitor`).
            // `1` iff non-null, a plain object and header class id ==
            // `class_id`; `0` otherwise. Total (a null answers `0` with no
            // load) and RAX-only: no call, no deopt, no safepoint.
            Op::ExactClassIs { class_id } => {
                // JIT round 18 wave 11 (lane osrfollow): fused into its
                // consuming `Op::If` (`prepare_fusion_tables`), which emits
                // the test on its own flags: nothing to materialise here.
                if self.fused_cmp.get(id as usize).copied().unwrap_or(false) {
                    return;
                }
                let class_id = *class_id;
                let slot = self.alloc_slot(id);
                let null_done = if self.receiver_already_non_null(node.inputs[0]) {
                    self.gp_load_value(RAX, node.inputs[0]);
                    None
                } else {
                    self.gp_load_value(RAX, node.inputs[0]);
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    // JZ done: RAX is already the `0` answer.
                    Some(self.emit_jcc_rel32(0x84))
                };
                // An array's header carries its COMPONENT's class id, so the
                // kind byte decides first (the same test the megamorphic PIC
                // makes): TEST BYTE [RAX + KIND_TAGS_BYTE_OFFSET],
                // KIND_TAG_BYTE_MASK -- kind bits only.
                const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
                self.buf.emit(&[
                    0xF6,
                    0x40,
                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
                    cratonvm_types::KIND_TAG_BYTE_MASK,
                ]);
                let not_plain = self.emit_jcc_rel32(0x85); // JNE miss
                                                           // CMP DWORD [RAX+0], class_id — the `Op::Unbox` guard's form.
                self.buf.emit(&[0x81, 0x78, 0x00]);
                self.buf.emit(&class_id.to_le_bytes());
                let wrong_class = self.emit_jcc_rel32(0x85); // JNE miss
                self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // MOV EAX, 1
                let hit_done = self.emit_jmp_rel32();
                self.patch_rel32_to_here(not_plain);
                self.patch_rel32_to_here(wrong_class);
                self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
                if let Some(p) = null_done {
                    self.patch_rel32_to_here(p);
                }
                self.patch_rel32_to_here(hit_done);
                self.store_rax(slot);
            }
            // JIT round 19 proposal wave (lane osr19, OSRPOLY19-2): the class
            // word of a polymorphic guard chain's receiver, loaded once at the
            // chain head (`IrBuilder::begin_bimorphic_receiver_splice` under
            // `CRATONVM_JIT_IR_CLASS_WORD_CHAIN`). The header class id of a
            // plain object, `0` for a null (no load) or an array. RAX-only:
            // no call, no deopt, no safepoint ([`class_word_of_bytes`]).
            Op::ClassWordOf => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                self.buf.emit(&class_word_of_bytes());
                self.store_rax(slot);
            }
            // Round 11 wave 14 (lane osrmerge): the opaque OSR merge
            // (`IrBuilder::build_opaque_osr_merge`). The flag is `0` wherever
            // it executes: the method-entry path never takes the arm, and the
            // entry stub jumps past the `If` into the arm itself.
            Op::OsrEntryFlag { .. }
            // JIT round 19 wave 5 (lane handler5, SPLICE4-1): the compiled
            // handler arm's flag, the same constant `0` (its `If` folds it).
            | Op::CatchEntryFlag { .. } => {
                let slot = self.alloc_slot(id);
                // RB25-1, step 2: when every reader is an `If` that
                // `constant_if_live_block` folds, nothing ever reads the
                // zero, so the `XOR; store` pair goes too (the
                // `CRATONVM_JIT_IR_CALL_THREW_FLAG_ELIDE` rule). The `If`
                // lowering refuses the attempt if one of them does not fold
                // after all (`flag_store_elided`).
                if self.jump_entered_flag_store_unread(id) {
                    if let Some(cell) = self.flag_store_elided.get_mut(id as usize) {
                        *cell = true;
                    }
                } else {
                    self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
                    self.store_rax(slot);
                }
            }
            // SPLICE4-1: the throwable the probe parked in the phi-copy
            // scratch word before jumping here (`emit_catch_probes`). Read
            // first in the arm by construction (`catch_arm_wire`), then the
            // word is cleared so no stale oop stays in the reserved tail.
            Op::CaughtException => {
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.phi_copy_scratch_slot_off);
                self.store_rax(slot);
                self.emit_zero_frame_slot(self.phi_copy_scratch_slot_off);
            }
            // JIT round 19 wave 3 (lane eager3, EAGER2-3): `1` when the class
            // init byte at `state_addr` reads `initialized`, else `0`. RAX-only:
            // no call, no deopt, no safepoint ([`class_init_test_bytes`]).
            Op::ClassInitTest {
                state_addr,
                initialized,
            } => {
                let slot = self.alloc_slot(id);
                self.buf
                    .emit(&class_init_test_bytes(*state_addr as u64, *initialized));
                self.store_rax(slot);
            }
            Op::VthreadBoundaryTest { offset, mask } => {
                let tls = crate::x64::jit_thread_tls_disp() as u32;
                if tls == 0 {
                    self.latch_bailout(Bailout::new(BailoutReason::Internal(
                        "virtual-thread guard without a thread mirror",
                    )));
                    return;
                }
                let slot = self.alloc_slot(id);
                self.emit_mov_rax_tls_disp32(tls);
                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                let no_thread = self.emit_jcc_rel32(0x84);
                self.buf.emit(&[0xF7, 0x80]); // TEST DWORD PTR [RAX + offset], mask
                self.buf.emit(&offset.to_le_bytes());
                self.buf.emit(&mask.to_le_bytes());
                self.buf.emit(&[0x0F, 0x94, 0xC0, 0x0F, 0xB6, 0xC0]); // SETZ AL; MOVZX EAX, AL
                let done = self.emit_jmp_rel32();
                self.patch_rel32_to_here(no_thread);
                self.emit_mov_reg_imm64(RAX, 1);
                self.patch_rel32_to_here(done);
                self.store_rax(slot);
            }
            // nio21 wave 19 (lane L3): the current thread's `java.lang.Thread`
            // mirror, the word `jit_thread_current_thread_direct`'s fast arm
            // returns: the `JvmThread*` from the `JIT_THREAD` TLS mirror, then
            // its `java_thread_obj` at `offset` (`None` = 0). `0` outside a JIT
            // thread context, or when this build has no TLS mirror; the
            // `ThreadLocal.get()` expansion that reads it sends a null to its
            // call. RAX-only: no call, no deopt, no safepoint.
            Op::CurrentThreadMirror { offset } => {
                let slot = self.alloc_slot(id);
                let tls = u32::try_from(crate::x64::jit_thread_tls_disp()).unwrap_or(0);
                let off = i32::try_from(*offset).unwrap_or(0);
                if tls == 0 || off == 0 {
                    self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
                } else {
                    self.emit_mov_rax_tls_disp32(tls);
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    let none = self.emit_jcc_rel32(0x84); // JZ .done (RAX = 0)
                    // MOV RAX, [RAX + disp32] -- `java_thread_obj`.
                    self.buf.emit(&[0x48, 0x8B, 0x80]);
                    self.buf.emit(&off.to_le_bytes());
                    self.patch_rel32_to_here(none);
                }
                self.store_rax(slot);
            }
            // JIT round 20 wave 11 (lane pIndy, SPLICE19-1): the object a VM
            // root word holds (a never-released JNI global reference box,
            // rewritten in place by the collector while mutators are parked).
            // RAX-only: no call, no deopt, no safepoint.
            Op::IndyRootRef { slot_addr } => {
                let slot = self.alloc_slot(id);
                // Cast: a VM-lived root word's address as the imm64 operand.
                self.emit_mov_reg_imm64(RAX, *slot_addr as u64);
                self.buf.emit(&[0x48, 0x8B, 0x00]); // MOV RAX, [RAX]
                self.store_rax(slot);
            }
            // JVM local `i` out of the interpreter's locals array, whose
            // pointer `emit_osr_entry_stubs` parked in the phi-copy scratch
            // word for an opaque arm and did NOT clear. Nothing in the arm
            // writes that word before its edge copies run, and every
            // `OsrLocal` of the arm is pinned to it, so each is read before
            // the edge's parallel copy can use the scratch. RAX-only, like the
            // `Op::Param` arm it mirrors: the same home and register publish
            // per type.
            Op::OsrLocal(idx) => {
                let disp = (i32::from(*idx)) * 8;
                let slot = self.alloc_slot(id);
                self.load_to_rax(self.phi_copy_scratch_slot_off);
                // MOV RAX, [RAX + disp32]
                self.buf.emit(&[0x48, 0x8B, 0x80]);
                self.buf.emit(&disp.to_le_bytes());
                match node.ty {
                    IrType::Int | IrType::Long => self.gp_store_value(id, slot, RAX),
                    IrType::Ref => self.store_rax(slot),
                    IrType::Float | IrType::Double => {
                        self.store_rax(slot);
                        self.publish_fp_from_slot(id, slot, node.ty == IrType::Double);
                    }
                    _ => self.latch_bailout(Bailout::with_context(
                        BailoutReason::Internal("ir_lower: an OSR local of a non-value type"),
                        format!("n{id} {:?}", node.ty),
                    )),
                }
            }
            // A token: the interpreter's memory on OSR entry. Emits nothing.
            Op::OsrMemory => {}
            // nio20 L2 (NIO19-9): `Reference.reachabilityFence`. Emits
            // nothing: the node is the referent's last use, so liveness has
            // already kept it in every map up to here.
            Op::KeepAlive => {}
            // nio21 wave 3 L2: a call-site exception edge's flag. `0` wherever
            // it executes, like the OSR flag above: an armed site's `If`
            // transfers straight to its false edge without reading it
            // (`lower_terminator`) and the call's exception pad jumps past the
            // `If` into the true edge's block; an unarmed site's `If` reads
            // this zero and never takes the true edge.
            Op::CallThrew { .. } => {
                let slot = self.alloc_slot(id);
                // nio21 wave 4 L2 (proposal L2W3-3): an ARMED site's `If` is a
                // plain transfer to its false edge (`call_throw_targets`
                // proved both projections resolve), so nothing reads this
                // flag: the slot is kept (an `Int` home is never published)
                // and the two instructions on the call's normal path go.
                // `CRATONVM_JIT_IR_CALL_THREW_FLAG_ELIDE=0` keeps the store.
                let armed = node.inputs.get(1).is_some_and(|call| {
                    self.call_throw_plans.contains_key(call)
                        || self.implicit_throw_plans.contains_key(call)
                });
                if !armed {
                    self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
                    self.store_rax(slot);
                }
            }
            // nio21 wave 3 L2's `Op::CaughtException` (the throwable
            // `jit_local_handler_lookup` stored into the phi-copy scratch word,
            // read at the head of the block the call-site pad jumped into) is
            // the SPLICE4-1 arm above: the same read-then-clear of that word.
            Op::MonitorEnter | Op::MonitorExit => {
                // Round 14 wave 2 (lane syncsplice, SR-1): a synchronized
                // splice's pair is described by no frame state, so it is
                // emitted only while nothing that can trap is scheduled
                // between the two (`ir::SyncSpliceWindow`).
                if let Err(why) = self.sync_splice_window_refusal(id) {
                    // Round 14 wave 3 (lane fixup, RV3-1): name the splice, so
                    // the IR door rebuilds without it instead of losing the
                    // method (`last_sync_window_refused_pc`).
                    if let Some(pc) = node.bytecode_pc {
                        note_sync_window_refused_pc(pc);
                    }
                    if crate::x64::dbg_jitc_enabled() {
                        eprintln!(
                            "[cratonvm-jitc] ir-sync-splice-window REFUSED bci={}: {why}",
                            self.cur_bci
                        );
                    }
                    self.latch_bailout(Bailout::with_context(
                        BailoutReason::UnsupportedShape("synchronized splice window"),
                        why,
                    ));
                    return;
                }
                // `[ctrl, mem, obj]`.
                let obj_id = node.inputs[2];
                let obj_slot = self.slot_of(obj_id);
                let enter = matches!(node.op, Op::MonitorEnter);
                // The uncontended case inline (round 11 wave 7, page
                // `r11w6-lock-ir-tier-monitor-still-calls-the-helper`), with
                // everything below as its slow path. Placed BEFORE the map and
                // joined AFTER the enter-only store-back, so the fast path
                // skips the sp-id store, the shadow push and its
                // `emit_shadow_reload` as a pair (the balance check counts
                // emitted pushes and reloads, one each, on the slow path), the
                // frame-record republish and the sentinel test. Nothing moved
                // on a path that made no call, so the store-back is not owed.
                // The node is still a call/safepoint in the machine model, so
                // no value is register-resident in the five registers the
                // fast path clobbers.
                let thin_lock_done = self.emit_monitor_fast_path(enter, obj_slot);
                // Interpreter round i2 wave 58 (lane L2): a synchronized
                // splice's enter publishes a map whose inline chain names the
                // spliced callee at bci 0, so a thread BLOCKED here lists it
                // on top, as HotSpot's (and the interpreter's frame-first
                // door's) does. `None` for every other monitor op.
                let callee_base = if enter {
                    self.sync_splice_enter_callee_base(id)
                } else {
                    None
                };
                self.sp_chain_pc_override = callee_base;
                let sp_id_before = self.next_sp_id;
                // JIT round 21, lane oom: an armed enter's held dead-home
                // clears, on the slow path only (`alloc_site_armed`). Writes
                // no register and no flag. Nothing is held for an exit or an
                // unarmed enter, so those emit nothing here.
                self.emit_pending_call_clears();
                // Publish the map BEFORE the call, same contract as `Op::Call`:
                // both monitor ops carry `safepoint: true`, and a contended
                // acquire parks this thread for an entire collection.
                let sp_live_hi = self.spill_high_water;
                let mut map = self.emit_safepoint_map(sp_live_hi);
                self.sp_chain_pc_override = None;
                // The id the slot holds once the acquire RETURNED: the same
                // map under the caller's own chain, so a later reader of a
                // stale slot (an implicit trap that stores no id) is not told
                // the callee is still being entered.
                let after_enter_id = match callee_base {
                    Some(_) if self.next_sp_id != sp_id_before => {
                        self.record_safepoint_twin(sp_id_before)
                    }
                    _ => None,
                };
                let target = if matches!(node.op, Op::MonitorEnter) {
                    self.monitor_enter
                } else {
                    self.monitor_exit
                };
                // `scan_frame_needs` reserves the slot for every monitor op; a
                // zero offset here would pass `[rbp]` (a stack address) as the
                // helper's VM pointer, so refuse the method rather than emit it.
                if self.context_slot_off == 0 {
                    self.latch_bailout(Bailout::new(BailoutReason::Internal(
                        "ir_lower: monitor op without a reserved VM context slot",
                    )));
                }
                let return_address = crate::runtime_lowering::emit_monitor_stub(
                    &mut self.buf,
                    self.context_slot_off,
                    obj_slot,
                    target,
                    self.frame_record,
                );
                // A thread dump taken while blocked in `monitorenter` inside a
                // spliced callee names the inlined frames too.
                self.anchor_safepoint_return(map.as_mut(), return_address);
                self.note_inline_frame_return_site_at(return_address);
                self.emit_identity_after_shared_stub();
                // A `MOV [rbp - off], imm32`: no register, no flag.
                if let Some(twin) = after_enter_id {
                    self.emit_sp_id_store(twin);
                }
                // Retract the push the map above emitted, exactly as
                // `Op::New` does. Without it `lower_inner`'s push/reload
                // balance check refused every method with a synchronized block
                // — after emitting the whole body. The reload works through
                // RCX, so the helper's RAX survives for the sentinel test.
                self.emit_shadow_reload();
                // `i64::MIN` means the helper published a pending Java
                // exception (a null receiver takes the NPE path).
                self.emit_cmp_rax_sentinel();
                self.buf.emit(&[0x0F, 0x81]); // JNO ok
                let ok_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.buf.emit_byte(0xE9); // JMP shared exception epilogue
                let exception_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                // Round 15 wave 1 (lane sync, SS8-2): a self-locking body's own
                // enter / exit fails holding nothing to release (the enter
                // took nothing; a failed exit is not retried), so its edge
                // leaves through the stub that does not release.
                if self
                    .self_lock
                    .as_ref()
                    .is_some_and(|lock| lock.names_op(id))
                {
                    self.push_self_lock_unheld_exc_patch(exception_patch);
                } else {
                    self.push_call_exc_patch(exception_patch);
                }
                let ok = self.buf.pos();
                let rel = ok as i32 - (ok_patch as i32 + 4);
                Self::patch_or_bail(&mut self.buf, ok_patch, rel);
                // Store the possibly-REMAPPED reference back — after ENTER only.
                //
                // Belt and braces, not load-bearing. `jit_monitor_enter` answers
                // with `monitor_enter_blocking(..)`'s object, i.e. the
                // post-move address, and `conservative_roots::remap_one_jit_frame`
                // rewrites published slots keyed on their CURRENT value and
                // reaches the same address — so this store writes a value the
                // slot already holds. It is kept because it is the one arm that
                // does not depend on the publication being right.
                //
                // The whole determination lives once, in
                // `crate::runtime_lowering::emit_monitor_stub`'s doc; `jit-api`'s
                // `monitor_enter` row and `x64/op_object.rs` both point there.
                // Do not restate it here.
                //
                // `jit_monitor_exit` returns `1` on success, not the object,
                // which is why this is enter-only: storing that `1` into the
                // receiver's home made a second `synchronized (this)` read
                // `this` as the address 1, and the oop map published it.
                if enter {
                    self.store_rax(obj_slot);
                }
                if let Some(done) = thin_lock_done {
                    self.patch_rel32_to_here(done);
                }
            }
            Op::New {
                class_id,
                num_fields,
            } => 'new: {
                // `jit_new_object` can trigger a real collection (TLAB
                // exhaustion), so this is a GC-capable point and owes the same
                // map `Op::NewArray` / `Op::Call` / `Op::MonitorEnter` pay.
                //
                // It did not pay it, on the reasoning recorded above the
                // `Op::NewArray` arm -- "unlike `Op::New`'s arm, which has no
                // operand of its own to protect". That reads the map as
                // protection for the NODE, and it is not: it describes every
                // live reference IN THE FRAME at this program point. A frame
                // parked inside the allocation therefore had its sp-id slot
                // still naming whichever earlier safepoint last wrote it, or
                // the prologue sentinel when none had.
                //
                // MEASURED on `org.h2.test.jdbc.TestCachedQueryResults`: with
                // the poll repair in place the last surviving `no-map-for-id`
                // frame was `java/util/ArrayList.iterator()` -- a method whose
                // whole body is `new Itr(this)` -- reading `sp_id=0` against
                // `maps=2 ids=[1, 2]`. It was parked in this stub.
                //
                // Ordered exactly as `Op::NewArray` orders it: the map is taken
                // BEFORE `alloc_slot`, which marks this node's own result slot
                // defined immediately, because the result is not written until
                // the stub returns and publishing it early hands the collector
                // an uninitialised word to treat as a live reference.
                //
                // Round 13 wave 8 (lane iropt7): `tlab_announce_classes` holds
                // the COMPILING method's own `new` sites only, so a `new` in a
                // spliced body (`HashMap.newNode`'s `new Node` inside `putVal`)
                // paid the `jit_post_tlab_init` call -- and, with it, the map
                // before the bump -- on every allocation. The same no-op proof
                // for such a site: see `spliced_new_post_init_is_noop`.
                // JIT round 20 wave 12 (lane qLamNew,
                // `CRATONVM_JIT_IR_INDY_CAPTURE_NEW`): a capturing lambda's
                // proxy (`Graph::lambda_proxy_alloc_classes`) has no `<clinit>`
                // and owes no post-init work -- the builder stores every
                // capture right after this allocation.
                // Every route (inline bump, `jit_new_object`) allocates by this
                // predicate; a legacy object must not be recorded compact-proven.
                let allocates_compact =
                    cratonvm_types::compact_tlab_total_size(*class_id, *num_fields).is_some();
                let lambda_proxy = self.graph.lambda_proxy_alloc_classes.contains(class_id);
                let post_init_noop = lambda_proxy
                    || self.tlab_announce_classes.contains(class_id)
                    || (node
                            .bytecode_pc
                            .is_some_and(|pc| self.pc_is_in_a_spliced_body(pc))
                        && spliced_new_post_init_is_noop(*class_id, *num_fields));
                let tlab_plan = crate::runtime_lowering::InlineTlabPlan {
                    thread_slot_off: self.shadow_thread_slot_off,
                    thread_tls_disp: self.ir_tlab_thread_tls_disp(),
                    cursor_off: self.tlab_cursor_off,
                    end_off: self.tlab_end_off,
                    post_init: self.tlab_post_init,
                    context_off: self.context_slot_off,
                    class_id: *class_id,
                    num_fields: *num_fields,
                    announce: if post_init_noop
                        && cratonvm_types::jit_tlab_registration_required() {
                        self.zgc_note_tlab_object
                    } else {
                        0
                    },
                    // The single-pass tier's `TlabPostInit::Skip`, which this
                    // tier never had: a class with no primitive default and no
                    // finalizer, on a collector that finds objects by walking
                    // the chunk, gets nothing from `tlab_post_init` that the
                    // inline header stores above did not already write. See
                    // `ir_tlab_skip_post_init_enabled`.
                    skip_post_init: post_init_noop
                        && !cratonvm_types::jit_tlab_registration_required(),
                    // zt t10/ca2 (`CRATONVM_ZGC_JIT_POST_INIT_REGISTER_FIRST`,
                    // opt-in): a class that still owes `tlab_post_init` on ZGC
                    // registers before the commit. A no-op class is the
                    // announce plan's (or, under `CRATONVM_JIT_IR_ZGC_ANNOUNCE=0`,
                    // that kill switch's plain order).
                    register_first: if !post_init_noop
                        && cratonvm_types::jit_tlab_registration_required()
                        && crate::x64::zgc_post_init_register_first_enabled()
                    {
                        self.zgc_note_tlab_object
                    } else {
                        0
                    },
                    // JIT round 21 wave 2 (lane alloc,
                    // `CRATONVM_JIT_IR_ZGC_POST_INIT_HELPER_FIRST`, default ON):
                    // on ZGC the `jit_post_tlab_init` arm calls before it
                    // commits, as the announce arm does. Only for a class this
                    // compile PROVED finalizer- and reference-free
                    // (`finalizer_free_new_classes`): the helper must not
                    // allocate from this thread's TLAB while the cursor is
                    // uncommitted, and finalizer registration could.
                    post_init_helper_first: cratonvm_types::jit_tlab_registration_required()
                        && self.graph.finalizer_free_new_classes.contains(class_id)
                        && crate::runtime_lowering::zgc_post_init_helper_first_enabled(),
                };
                let mapped = Self::ir_gc_point_maps_enabled();
                // The map on the SLOW PATH only (`CRATONVM_JIT_IR_ALLOC_MAP_SINK`,
                // default on), the single-pass tier's allocation-spill sink in
                // this tier. With `skip_post_init` the inline fast path makes no
                // call at all between here and the merge, so no collector can
                // observe this frame on it, and the map -- which in a frame-block
                // body is the block's FIRST publication, a dozen stores -- was
                // paid by every allocation for nothing. Later safepoints test the
                // block's published flag at run time, so a fast path that skipped
                // it is published by the next one that needs it.
                //
                // The map is still built BEFORE `alloc_slot`, in compile order,
                // so this node's own unwritten result slot is not in it.
                //
                // JVMS 6.5: `new` initialises its class, and only the stub's
                // `jit_new_object` runs `<clinit>` -- the bump's
                // `jit_post_tlab_init` / announce helper do not. So either bump
                // is for a class PROVEN initialised at compile time only; see
                // `Self::new_class_proven_initialized`.
                let proven_initialized =
                    lambda_proxy || self.new_class_proven_initialized(*class_id, node.bytecode_pc);
                let mut deferred_slow: Vec<usize> = Vec::new();
                let deferred_gen = mapped
                    && proven_initialized
                    && ir_inline_tlab_enabled()
                    && crate::runtime_lowering::emit_inline_tlab_fast_path_ir(
                        &mut self.buf,
                        &tlab_plan,
                        self.new_object,
                        &mut deferred_slow,
                    );
                // JIT round 22 wave 2, lane memory (MEMORY22-1,
                // `CRATONVM_JIT_IR_ALLOC_MAP_SINK_ZGC`, opt-in): on ZGC the
                // announced bump defers its map too. Its fast path's only call
                // is the thin announce helper, a leaf that runs no Java and
                // parks nowhere (`jit_zgc_note_tlab_object`: the start-registry
                // insert, the young grain, allocate-black -- atomics), and a
                // pause never walks a thread whose RIP is in Rust code (the
                // take-over resumes it to arrive at a poll, `xt_root_scan`), so
                // no collector observes this frame between the bump and the
                // next safepoint. The split arm (`zgc_alloc_tail_admitted`)
                // keeps its own route. The fast path joins BEFORE the null
                // test: the helper's `0` takes the exception path, as in the
                // default arm.
                let deferred_zgc = !deferred_gen
                    && mapped
                    && proven_initialized
                    && ir_inline_tlab_enabled()
                    && ir_alloc_map_sink_zgc_enabled()
                    && tlab_plan.announce != 0
                    && !tlab_plan.skip_post_init
                    && !(crate::x64::zgc_inline_arm_fast_join_enabled()
                        && self.zgc_alloc_tail_admitted())
                    && crate::runtime_lowering::emit_inline_tlab_new_zgc_fast_path_ir(
                        &mut self.buf,
                        &tlab_plan,
                        self.new_object,
                        self.frame_record,
                        &mut deferred_slow,
                    );
                if deferred_zgc && false {
                    eprintln!(
                        "[cratonvm-jitc] ir zgc alloc map sink: new bci={}",
                        self.cur_bci
                    );
                }
                let deferred = deferred_gen || deferred_zgc;
                // Round 13 wave 8 (lane iropt7): the deferred fast path's join
                // lands at `allocated` below, past the identity republish and
                // the null test. It made no call, so no stub zeroed the
                // frame's published identity, and its RAX is the bumped
                // address, never null. Only the slow path's stub owes both.
                // `CRATONVM_JIT_IR_ALLOC_FAST_JOIN=0` joins before them again.
                let mut fast_join: Option<usize> = None;
                let fast_join_ok = self.zgc_inline_arm_fast_join_admitted(tlab_plan.announce);
                let zgc_split_sunk = !deferred
                    && mapped
                    && fast_join_ok
                    && ir_inline_tlab_enabled()
                    && ir_alloc_map_sink_zgc_enabled()
                    && proven_initialized
                    && !tlab_plan.skip_post_init
                    && self.zgc_alloc_tail_admitted()
                    && !self.frame_block.is_empty()
                    && self.frame_block_shared;
                // JIT round 20 wave 17, lane gcDead (I-9): an armed site's
                // held clears go where only the helper call follows -- the
                // deferred slow path, or in front of a stub-only route --
                // and every other route gives the site up
                // (`disarm_alloc_site`). Nothing is held off the site, so an
                // unarmed `new` emits the bytes it always did.
                // JIT round 21, lane oom: under
                // `CRATONVM_JIT_IR_HELPER_SITE_DEAD_HOME_CLEAR` the post-init
                // bump (every ZGC `new`) keeps the
                // site and zeroes in front of its map on BOTH paths: its fast
                // path already makes a helper call, so the few stores are
                // noise there, and every GC point of the route follows.
                // `new_site_clear_placement` is the one rule for all four
                // switch combinations (`jr21_alloc_oom_clear_tests`).
                match new_site_clear_placement(
                    deferred || zgc_split_sunk,
                    ir_inline_tlab_enabled() && proven_initialized,
                    self.helper_site_dead_home_clear,
                ) {
                    NewSiteClears::SlowArms => {}
                    NewSiteClears::Disarm => self.disarm_alloc_site(id),
                    NewSiteClears::FrontOfMap => self.emit_pending_call_clears(),
                }
                let mut map = None;
                let mut zgc_split_slow_map_bytes = Vec::new();
                let mut zgc_split_ensure_frame_block = false;
                if deferred_gen && self.gen_alloc_tail_admitted() {
                    let map_start = self.buf.mark();
                    let maps_before = self.oop_maps.len();
                    self.emit_pending_call_clears();
                    let sp_live_hi = self.spill_high_water;
                    self.defer_frame_block_ensure = true;
                    map = self.emit_safepoint_map(sp_live_hi);
                    self.defer_frame_block_ensure = false;
                    let map_end = self.buf.pos();
                    let slow_map_bytes = self.buf.as_slice()[map_start.len..map_end].to_vec();
                    self.buf.rewind_to_mark(map_start);
                    let ensure_frame_block = self
                        .oop_maps
                        .get(maps_before)
                        .is_some_and(|m| !m.frame_slot_offsets.is_empty());
                    let exit_bci = self.note_deferred_cross_call_exit();
                    let stub_target = self.new_object;
                    self.deferred_gen_alloc_tails.push(DeferredGenAllocTail {
                        map,
                        slow: deferred_slow,
                        slow_map_bytes,
                        ensure_frame_block,
                        kind: DeferredGenAllocKind::New {
                            class_id: *class_id,
                            num_fields: *num_fields,
                            stub_target,
                        },
                        resume: self.buf.pos(),
                        exit_bci,
                        node_pc: self.cur_node_pc,
                        site_bci: self.cur_bci,
                    });
                    let slot = self.alloc_slot(id);
                    self.store_rax(slot);
                    if let Some(block) = self.field_proof_block() {
                        if allocates_compact {
                            self.compact_proven.push((id, block));
                        }
                        self.layout_proven.push((id, block));
                    }
                    break 'new;
                }
                if deferred {
                    let join = self.emit_jmp_rel32();
                    for patch in deferred_slow {
                        self.patch_rel32_to_here(patch);
                    }
                    self.emit_pending_call_clears();
                    let sp_live_hi = self.spill_high_water;
                    map = self.emit_safepoint_map(sp_live_hi);
                    let slow_return = crate::runtime_lowering::emit_new_object_stub(
                        &mut self.buf,
                        self.context_slot_off,
                        self.new_object,
                        *class_id,
                        *num_fields,
                        self.frame_record,
                    );
                    self.anchor_safepoint_return(map.as_mut(), slow_return);
                    // The same inline-frame row the non-deferred stub gets.
                    self.note_inline_frame_return_site_at(slow_return);
                    self.emit_shadow_reload();
                    if !deferred_zgc {
                        fast_join = Some(join);
                    } else {
                        self.patch_rel32_to_here(join);
                    }
                } else if zgc_split_sunk {
                    let map_start = self.buf.mark();
                    let maps_before = self.oop_maps.len();
                    self.emit_pending_call_clears();
                    let sp_live_hi = self.spill_high_water;
                    self.defer_frame_block_ensure = true;
                    map = self.emit_safepoint_map(sp_live_hi);
                    self.defer_frame_block_ensure = false;
                    let map_end = self.buf.pos();
                    zgc_split_slow_map_bytes = self.buf.as_slice()[map_start.len..map_end].to_vec();
                    self.buf.rewind_to_mark(map_start);
                    zgc_split_ensure_frame_block = self
                        .oop_maps
                        .get(maps_before)
                        .is_some_and(|m| !m.frame_slot_offsets.is_empty());
                } else if mapped {
                    let sp_live_hi = self.spill_high_water;
                    map = self.emit_safepoint_map(sp_live_hi);
                }
                let slot = self.alloc_slot(id);
                // Inline TLAB bump first, with the stub as its slow path.
                // Declining emits the stub alone, which is the behaviour this
                // arm had before the bump existed.
                //
                // The safepoint map above still stands: the bump itself cannot
                // collect, but its slow path is the same stub, and a map is a
                // statement about the FRAME at this program point rather than
                // about which arm runs.
                // zr s1/ca (`CRATONVM_JIT_ZGC_INLINE_ARM_FAST_JOIN`): the ZGC
                // inline start-bit arm made no call, so it lands at
                // `allocated` below, past the identity store and the null
                // test only the stub owes -- at a site whose shadow reload
                // emits nothing (no push to retract on that arm).
                let mut zgc_fast_join: Option<usize> = None;

                // JIT round 21, lane cf: which route this site took, for the
                // `ir new-site` line below (`CRATONVM_DBG_JITC=1`).
                let mut new_site_route: &'static str = if deferred { "sunk" } else { "stub-only" };
                if !deferred {
                    // zs s9/ca4 (`CRATONVM_JIT_ZGC_INLINE_ARM_FALL_THROUGH`):
                    // the fast half here, its inline arm falling through to
                    // the result store; the cold half after the body.
                    if fast_join_ok
                        && ir_inline_tlab_enabled()
                        && proven_initialized
                        && !tlab_plan.skip_post_init
                        && self.zgc_alloc_tail_admitted()
                    {
                        if let Some(split) = crate::runtime_lowering::emit_inline_tlab_new_ir_split(
                            &mut self.buf,
                            &tlab_plan,
                            self.new_object,
                        ) {
                            let stub_target = self.new_object;
                            self.defer_zgc_alloc_tail(
                                DeferredZgcAllocKind::New {
                                    plan: tlab_plan,
                                    stub_target,
                                    slow_map_bytes: zgc_split_slow_map_bytes,
                                    ensure_frame_block: zgc_split_ensure_frame_block,
                                },
                                split,
                                map,
                            );
                            if false {
                                eprintln!("[cratonvm-jitc] ir zgc inline-arm fall-through: new");
                            }
                            // Round rx27 wave 1 (lane alloc): this route left
                            // before the `ir new-site` line below and printed
                            // no census of its own.
                            if crate::x64::dbg_jitc_enabled() {
                                let spliced = node
                                    .bytecode_pc
                                    .is_some_and(|pc| self.pc_is_in_a_spliced_body(pc));
                                let start_bit = crate::x64::ir_zgc_inline_announce_enabled();
                                let call = new_site_call_reason(NewSiteFacts {
                                    inline_tlab: true,
                                    proven_initialized,
                                    bump_emitted: true,
                                    post_init_noop,
                                    spliced,
                                    skip_post_init: false,
                                    announce: true,
                                    inline_start_bit: start_bit,
                                });
                                eprintln!(
                                    "[cratonvm-jitc] ir new-site n{id} class={class_id} \
                                     fields={num_fields} route=zgc-split \
                                     post_init_noop={post_init_noop} announce=true \
                                     skip_post_init=false proven_init={proven_initialized} \
                                     spliced={spliced} mapped={mapped} call={call} method={}",
                                    self.dbg_method(),
                                );
                            }
                            self.store_rax(slot);
                            if let Some(block) = self.field_proof_block() {
                                if allocates_compact {
                                    self.compact_proven.push((id, block));
                                }
                                self.layout_proven.push((id, block));
                            }
                            break 'new;
                        }
                        if zgc_split_sunk {
                            self.buf.emit(&zgc_split_slow_map_bytes);
                            if zgc_split_ensure_frame_block {
                                self.emit_frame_block_ensure();
                            }
                        }
                    }
                    let inline_slow_return = if ir_inline_tlab_enabled() && proven_initialized {
                        if fast_join_ok {
                            crate::runtime_lowering::emit_inline_tlab_new_ir_fast_join(
                                &mut self.buf,
                                &tlab_plan,
                                self.new_object,
                                self.frame_record,
                                &mut zgc_fast_join,
                            )
                        } else {
                            crate::runtime_lowering::emit_inline_tlab_new_ir(
                                &mut self.buf,
                                &tlab_plan,
                                self.new_object,
                                self.frame_record,
                            )
                        }
                    } else {
                        None
                    };
                    if inline_slow_return.is_some() {
                        new_site_route = if tlab_plan.skip_post_init {
                            "inline-mapped"
                        } else if tlab_plan.announce != 0 {
                            "inline-announce-call"
                        } else {
                            "inline-post-init-call"
                        };
                    }
                    // An `OutOfMemoryError` / `ExceptionInInitializerError`
                    // raised in the helper inside a spliced callee keeps the
                    // inlined frames in its stack trace. The bump's slow path is
                    // the same stub CALL and gets the same row (it used to
                    // record none).
                    let return_address = match inline_slow_return {
                        Some(slow_return) => slow_return,
                        None => {
                            crate::runtime_lowering::note_stub_only_alloc();
                            crate::runtime_lowering::emit_new_object_stub(
                                &mut self.buf,
                                self.context_slot_off,
                                self.new_object,
                                *class_id,
                                *num_fields,
                                self.frame_record,
                            )
                        }
                    };
                    self.anchor_safepoint_return(map.as_mut(), return_address);
                    self.note_inline_frame_return_site_at(return_address);
                }
                // JIT round 21, lane cf (`jr21-cf-completable-future-cost-split-20261014.md`):
                // one line per compiled `new`, naming the route and the facts
                // that chose it. `inline-post-init-call` / `inline-announce-call`
                // pay a Rust call on EVERY allocation; `stub-only` pays the
                // `jit_new_object` call. Diagnostic only.
                // Round rx27 wave 1 (lane alloc): `call=` names why the site
                // pays a Rust call per allocation (`none`: its fast path makes
                // none; `new_site_call_reason`), and `method=` the compile,
                // since background compiles interleave their lines.
                if crate::x64::dbg_jitc_enabled() {
                    let spliced = node
                        .bytecode_pc
                        .is_some_and(|pc| self.pc_is_in_a_spliced_body(pc));
                    let call = new_site_call_reason(NewSiteFacts {
                        inline_tlab: ir_inline_tlab_enabled(),
                        proven_initialized,
                        bump_emitted: new_site_route != "stub-only",
                        post_init_noop,
                        spliced,
                        skip_post_init: tlab_plan.skip_post_init,
                        announce: tlab_plan.announce != 0,
                        inline_start_bit: crate::x64::ir_zgc_inline_announce_enabled(),
                    });
                    eprintln!(
                        "[cratonvm-jitc] ir new-site n{id} class={class_id} fields={num_fields} \
                         route={new_site_route} post_init_noop={post_init_noop} \
                         announce={} skip_post_init={} proven_init={proven_initialized} \
                         spliced={spliced} mapped={mapped} call={call} method={}",
                        tlab_plan.announce != 0,
                        tlab_plan.skip_post_init,
                        self.dbg_method(),
                    );
                }
                // Every shape ends in the shared republish that zeroes the id.
                self.emit_identity_after_shared_stub();
                // Retract the push the map above emitted. Unbalanced pushes are
                // not a leak this backend tolerates: `lower_inner` compares
                // `shadow_pushes` against `shadow_reloads` and refuses the whole
                // method. RCX, never RAX -- the returned (possibly relocated)
                // pointer is untouched.
                if mapped && !deferred {
                    self.emit_shadow_reload();
                }

                // `jit_new_object` returns null after publishing a pending
                // initialization/OOM exception. Convert that private sentinel
                // to the JIT-wide i64::MIN return before entering the shared
                // exception epilogue, matching the baseline tier.
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
                // allocation success -- tolerated on an overflowed buffer; see
                // `Self::patch_or_bail` / `patch_rel32_to_here`.
                Self::patch_or_bail(&mut self.buf, allocated_patch, rel);
                if let Some(join) = fast_join {
                    self.patch_rel32_to_here(join);
                }
                if let Some(join) = zgc_fast_join {
                    self.patch_rel32_to_here(join);
                    if false {
                        eprintln!("[cratonvm-jitc] ir zgc inline-arm fast join: new");
                    }
                }
                self.store_rax(slot);
                if let Some(block) = self.field_proof_block() {
                    if allocates_compact {
                        self.compact_proven.push((id, block));
                    }
                    self.layout_proven.push((id, block));
                }
            }
            // cov-06: array allocation. Inputs `[ctrl, mem, length]` — the
            // shared allocation stub differs from `Op::New`'s only in that
            // the third argument is a RUNTIME slot (the length) rather than
            // an immediate, and the target/immediate pair is chosen by
            // shape: `element_type != 0` is a `newarray` (the atype IS the
            // immediate, `helpers.newarray`); `element_type == 0` is an
            // `anewarray` of an already-loaded class (`component_class_id`
            // is the immediate, `helpers.anewarray_object`). Same
            // zero-on-failure convention as `Op::New` — `jit_newarray` /
            // `jit_anewarray_object` return `0` after publishing a pending
            // exception (OOM OR a negative length, both routed through the
            // interpreter's `NegativeArraySizeException`/`OutOfMemoryError`
            // machinery), converted to the JIT-wide `i64::MIN` sentinel
            // exactly like `Op::New`'s failure path.
            //
            // `jit_newarray`/`jit_anewarray_object` can trigger a real
            // collection (TLAB exhaustion), so this allocation
            // needs a fresh safepoint map published BEFORE it, exactly as
            // `Op::MonitorEnter`/`Op::Call` do: without one, `sp_id_slot_off`
            // keeps naming whichever EARLIER safepoint last wrote it (or none
            // at all), so a collection during THIS call matches a map
            // describing a different program point and relocates against it
            // — `emit_safepoint_map`'s own doc names this exact hazard.
            // Reached in practice: a hot method that `newarray`s in a tight
            // loop and returns the array to an interpreter caller corrupted
            // the returned reference under GC pressure before this map was
            // added (`vm/tests/jit_cov06_array_allocation.rs`, `gcRootsOK`/
            // the plain allocate-loop warm-up both reproduced it).
            //
            // Published BEFORE `alloc_slot(id)` too — `alloc_slot` marks this
            // node's OWN result slot `defined_nodes[id] = true` immediately,
            // and the result is not written until the call returns, so a map
            // taken after `alloc_slot` would hand the collector an
            // uninitialised word to treat as a live reference (the same
            // ordering `emit_safepoint_map`'s doc requires).
            Op::NewArray {
                element_type,
                component_class_id,
            } => 'newarray: {
                // JIT round 22 wave 1, lane memory (ALLOC21-1,
                // `CRATONVM_JIT_IR_NEWARRAY_MAP_SINK`, opt-in): on Generational /
                // G1 the map moves to the bump's slow path. Lowers the whole
                // node, or emits at most a length staging store and declines.
                if self.try_lower_newarray_map_sink(
                    id,
                    node.inputs[2],
                    *element_type,
                    *component_class_id,
                ) {
                    break 'newarray;
                }
                // Whether the inline bump is offered at all (the terms are
                // explained where they were read before, below); pure flag and
                // field reads, hoisted above the map for the stub-only clears.
                let registration = cratonvm_types::jit_tlab_registration_required();
                let ref_inline = *element_type == 0
                    && (registration || crate::runtime_lowering::ir_inline_anewarray_any_gc_enabled())
                    && crate::x64::inline_tlab_anewarray_enabled();
                let inline_offered = (*element_type != 0 || ref_inline)
                    && ir_inline_tlab_enabled()
                    && if registration {
                        crate::x64::zgc_inline_array_announce_enabled()
                            && self.zgc_note_tlab_object != 0
                    } else {
                        true
                    };
                // JIT round 21, lane smallfix (REVJIT221-2, 574bc22865;
                // re-landed by t1 wave 8 lane RL-MISC; I-9 placement,
                // unconditional): a route that is certainly stub-only (no bump
                // offered) is all slow path, so an armed site's held clears go
                // IN FRONT of the map, as on the sunk route
                // (`try_lower_newarray_map_sink`); after it, the map's shadow
                // push / reload copied a dead home's old value back over its
                // clear (retention only). An offered bump keeps them on its
                // slow path (`emit_newarray_bump_with_slow_path_clears`), and a
                // bump that declines still clears after the map, in front of
                // its stub. An unarmed site holds nothing: no bytes change.
                if !inline_offered {
                    self.emit_pending_call_clears();
                }
                let sp_live_hi = self.spill_high_water;
                let mut map = self.emit_safepoint_map(sp_live_hi);
                let slot = self.alloc_slot(id);
                // Round 9: the stub reads the length from a FRAME word, and a
                // length whose home was dropped (register-authoritative — an
                // exclusive register, e.g. `new int[n * 2]`) has none, so
                // `slot_of` refused and the whole method fell to the
                // single-pass backend (the refusal `slot_of_checked`'s own
                // comment records for `StringRegexOnly.run`). Stage it in the
                // phi-copy scratch word instead: a reserved bookkeeping word
                // below the spill band that only `emit_phi_copies` uses, and
                // only while an edge's copies are being emitted, never across
                // a node. Loaded AFTER the map above, whose shadow push
                // clobbers RAX; the residency file is callee-saved.
                let len = node.inputs[2];
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
                let (target, immediate) = if *element_type != 0 {
                    (self.newarray, u32::from(*element_type))
                } else {
                    (self.anewarray_object, *component_class_id)
                };
                // Round 16 wave 7 (lane strings): an inline TLAB bump for a
                // PRIMITIVE array under ZGC, announced like `Op::New`'s
                // (`CRATONVM_JIT_IR_ZGC_INLINE_ARRAY_ANNOUNCE`, default ON, `=0` kills it;
                // `CRATONVM_JIT_IR_INLINE_TLAB=0` kills it with the object
                // bump). Declining emits the stub alone, as before. The map
                // above stands for both arms: the fast path's announce call
                // cannot collect, and its slow path is the same stub.
                //
                // Strings round (`str16`, backlog `strings w7-2`): on a collector
                // that parses the chunk (Generational, G1) the same bump with
                // no announce (`announce: 0`), as the single-pass arm has always
                // done there (`CRATONVM_JIT_IR_INLINE_TLAB_NEWARRAY`, default ON,
                // `=0` is the kill switch). (`registration` is read above the map.)
                // zr s1/ca: the inline start-bit arm's join, landed at
                // `allocated` (see the `Op::New` arm).
                let mut zgc_fast_join: Option<usize> = None;
                // zr z3/ja: on ZGC an `anewarray` takes the same bump
                // (`CRATONVM_JIT_INLINE_TLAB_ANEWARRAY`, default ON, `=0`
                // kills it), its component class as the class word and the
                // stub's immediate. Never offered off ZGC.
                // JIT round 21 wave 2 (lane alloc,
                // `CRATONVM_JIT_IR_INLINE_TLAB_ANEWARRAY_ANY_GC`, opt-in): and on
                // a collector that parses the chunk (Generational, G1), with no
                // announce, as the primitive arm there. Every compiled
                // `new Object[16]` called `jit_anewarray_object` on those
                // collectors (`R21AllocLean lean-static-objarr16`: Gen 61 ns,
                // HotSpot 9). A zeroed body is all-null; a young TLAB object
                // owes no card or remembered-set entry. (`ref_inline` is read
                // above the map.)
                // zs s9/ca4: a split site's cold half, deferred past the body.
                let mut zgc_split: Option<(
                    DeferredZgcAllocKind,
                    crate::runtime_lowering::ZgcAllocSplit,
                )> = None;
                let inline_slow_return = if inline_offered {
                    let thread_tls_disp = self.ir_tlab_thread_tls_disp();
                    let array_plan = crate::runtime_lowering::InlineTlabArrayPlan {
                        thread_slot_off: self.shadow_thread_slot_off,
                        thread_tls_disp,
                        cursor_off: self.tlab_cursor_off,
                        end_off: self.tlab_end_off,
                        context_off: self.context_slot_off,
                        atype: *element_type,
                        length_off: length_slot,
                        announce: if registration {
                            self.zgc_note_tlab_object
                        } else {
                            0
                        },
                        component_class_id: (*element_type == 0).then_some(*component_class_id),
                        // zr s1/ca (`CRATONVM_JIT_IR_NEWARRAY_CONST_LENGTH`): a
                        // non-negative constant length, baked by the emitter on
                        // ZGC; the frame word above still holds it for the stub.
                        const_length: match self.graph.nodes.get(len as usize).map(|n| &n.op) {
                            Some(Op::Const(v)) => {
                                i32::try_from(*v).ok().and_then(|v| u32::try_from(v).ok())
                            }
                            _ => None,
                        },
                    };
                    // JIT round 20 wave 17, lane gcDead (I-9): an armed site
                    // takes the bump with the slow path in its own hands, so
                    // its held clears run there only.
                    let split = if !self.alloc_site_armed(id)
                        && array_plan.announce != 0
                        && crate::x64::zgc_inline_arm_fast_join_enabled()
                        && self.zgc_alloc_tail_admitted()
                    {
                        // zs s9/ca4: the fast half here, falling through to
                        // the result store below; the cold half after the body.
                        crate::runtime_lowering::emit_inline_tlab_newarray_ir_split(
                            &mut self.buf,
                            &array_plan,
                            target,
                        )
                    } else {
                        None
                    };
                    if let Some(split) = split {
                        zgc_split = Some((
                            DeferredZgcAllocKind::Array {
                                plan: array_plan,
                                stub_target: target,
                            },
                            split,
                        ));
                        None
                    } else if self.alloc_site_armed(id) {
                        self.emit_newarray_bump_with_slow_path_clears(
                            &array_plan,
                            target,
                            immediate,
                            length_slot,
                            &mut zgc_fast_join,
                        )
                    } else if self.zgc_inline_arm_fast_join_admitted(array_plan.announce) {
                        // zr s1/ca (`CRATONVM_JIT_ZGC_INLINE_ARM_FAST_JOIN`):
                        // the ZGC inline start-bit arm lands at `allocated`
                        // below, as `Op::New`'s does.
                        crate::runtime_lowering::emit_inline_tlab_newarray_ir_fast_join(
                            &mut self.buf,
                            &array_plan,
                            target,
                            self.frame_record,
                            &mut zgc_fast_join,
                        )
                    } else if self.gen_newarray_fast_join_admitted(array_plan.announce) {
                        // JIT r23 wave 11 (lane L3,
                        // `CRATONVM_JIT_IR_NEWARRAY_FAST_JOIN_ANY_GC`): off
                        // ZGC the fast path lands at `allocated` below too;
                        // the shared join variable is patched there.
                        crate::runtime_lowering::emit_inline_tlab_newarray_ir_gen_fast_join(
                            &mut self.buf,
                            &array_plan,
                            target,
                            self.frame_record,
                            &mut zgc_fast_join,
                        )
                    } else {
                        crate::runtime_lowering::emit_inline_tlab_newarray_ir(
                            &mut self.buf,
                            &array_plan,
                            target,
                            self.frame_record,
                        )
                    }
                } else {
                    None
                };
                if let Some((kind, split)) = zgc_split {
                    // zs s9/ca4: the inline arm fell through to here.
                    self.defer_zgc_alloc_tail(kind, split, map);
                    if false {
                        eprintln!("[cratonvm-jitc] ir zgc inline-arm fall-through: newarray");
                    }
                    self.note_newarray_site_census(
                        id,
                        *element_type,
                        *component_class_id,
                        inline_offered,
                        registration,
                        true,
                        "zgc-split",
                    );
                    self.store_rax(slot);
                    break 'newarray;
                }
                // Round rx27 wave 1 (lane alloc): one census line per site.
                let emitted = inline_slow_return.is_some();
                self.note_newarray_site_census(
                    id,
                    *element_type,
                    *component_class_id,
                    inline_offered,
                    registration,
                    emitted,
                    if emitted { "inline" } else { "stub-only" },
                );
                let return_address = match inline_slow_return {
                    Some(slow_return) => slow_return,
                    None => {
                        // I-9: a stub-only route is all slow path; an
                        // unarmed site holds nothing, so emits nothing here.
                        self.emit_pending_call_clears();
                        crate::runtime_lowering::emit_new_array_stub(
                            &mut self.buf,
                            self.context_slot_off,
                            target,
                            immediate,
                            length_slot,
                            self.frame_record,
                        )
                    }
                };
                self.anchor_safepoint_return(map.as_mut(), return_address);
                self.note_inline_frame_return_site_at(return_address);
                self.emit_identity_after_shared_stub();
                // The reload that PAIRS with the map above. Its absence was not
                // a missed optimisation: `emit_shadow_push` bumps the thread's
                // shadow top and `emit_shadow_reload` is what retracts it, so an
                // array allocation with any live reference around it leaked one
                // frame's worth of shadow slots per execution. The
                // `shadow_pushes != shadow_reloads` check at the end of
                // `lower_inner` caught it and refused the whole method — which
                // is why every C2 candidate containing `newarray` alongside a
                // live oop silently lost its optimized body, `Short2.<init>()V`
                // (`2 shadow pushes vs 1 reloads`) included.
                //
                // `Op::New` now emits the same pair for the same reason (see
                // its arm above); it used to emit neither. Placed exactly where
                // `Op::ConstString` / `Op::ConstClass` put theirs — after the stub, before the
                // zero test — because the reload uses RCX and never RAX, so the
                // returned (possibly relocated) pointer is untouched.
                self.emit_shadow_reload();

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
                if let Some(join) = zgc_fast_join {
                    self.patch_rel32_to_here(join);
                    if false {
                        eprintln!("[cratonvm-jitc] ir zgc inline-arm fast join: newarray");
                    }
                }
                self.store_rax(slot);
            }
            Op::Neg => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                match node.ty {
                    // FP negation = flip the IEEE sign bit of the bit pattern
                    // (correct for ±0.0 and NaN, unlike `0.0 - x`). Done on the
                    // integer pattern in RAX — no XMM needed. The float result's
                    // high 4 slot bytes are left zero; every consumer reads it
                    // back with MOVSS (4 bytes).
                    IrType::Float => {
                        // XOR EAX, 0x80000000
                        self.buf.emit_byte(0x35);
                        self.buf.emit(&0x80000000u32.to_le_bytes());
                    }
                    IrType::Double => {
                        // BTC RAX, 63 -- flip bit 63 in place. Round 11
                        // (irlower): was `MOV RCX, imm64; XOR RAX, RCX`, 13
                        // bytes and an RCX clobber for a one-bit flip; this is
                        // 5 bytes and writes only RAX (and CF, which nothing
                        // reads across a node boundary).
                        self.buf.emit(&[0x48, 0x0F, 0xBA, 0xF8, 0x3F]);
                    }
                    IrType::Int => {
                        // NEG EAX
                        self.buf.emit(&[0xF7, 0xD8]);
                    }
                    _ => {
                        // NEG RAX (Long)
                        self.buf.emit(&[0x48, 0xF7, 0xD8]);
                    }
                }
                self.store_rax(slot);
                // The FP arms computed in RAX, so an XMM-resident result
                // reaches its register through the home word it just wrote.
                if matches!(node.ty, IrType::Float | IrType::Double) {
                    self.publish_fp_from_slot(id, slot, node.ty == IrType::Double);
                }
            }
            // The three bitwise arms are width-aware for the same reason
            // `Add`/`Sub`/`Mul`/`Shl` are: an `IrType::Int` node must leave its
            // home word holding a value whose high half agrees with this tier's
            // int convention. A REX.W `AND`/`OR`/`XOR` over two operands that
            // disagree about that half — one sign-extended (`MOVSXD` out of
            // `iaload`, `F2I`, `LCmp`, a negative `Const`), one zero-extended
            // (any 32-bit ALU result) — produces a word that is neither the
            // sign- nor the zero-extension of its own low half, e.g.
            // `0xFFFF_FFFF_0000_0006`. `emit_array_null_bounds_guards_for`
            // validates only the low half while the SIB scales all 64, so such
            // a word was an out-of-bounds element address that passed its own
            // bounds check. The 32-bit form zero-extends through RAX in
            // hardware, which is the convention this tier documents, and is a
            // byte shorter. `gp_load_index_rcx` now enforces the same invariant
            // at the address-formation site (the index enters RCX zero-extended
            // through a 32-bit move); both are deliberate.
            Op::And => {
                // JIT rx27 wave 4 (lane regalloc): answered by its `If`'s
                // `TEST` (`plan_and_zero_tests`): nothing to materialise.
                if self.fused_test_and.get(id as usize).copied().unwrap_or(false) {
                    return;
                }
                // JIT rx27 wave 6: its home is bound here (the guard's frame
                // state describes it) and written in that guard's deopt pad
                // (`plan_and_zero_guard_tests`); nothing inline.
                if self.test_and_remat.get(id as usize).copied().unwrap_or(false) {
                    let _ = self.alloc_slot(id);
                    return;
                }
                if let Some(&raw) = self.elided_byte_mask_ands.get(&id) {
                    self.emit_elided_byte_mask_and(id, raw, node.ty);
                    return;
                }
                if self.fused_index_and.get(id as usize).copied().flatten().is_some() {
                    return;
                }
                let slot = self.alloc_slot(id);
                // W9-2 (`CRATONVM_JIT_IR_REG_ALLOC`): straight into its register.
                if self.reg_alloc_direct_alu(id, slot) {
                    return;
                }
                self.gp_load_value(RAX, node.inputs[0]);
                if !self.emit_alu_acc_imm(node.inputs[1], 0x25, node.ty != IrType::Int) {
                    // Round 17: or read in place from its register (`emit_alu_acc_reg`).
                    if !self.emit_alu_acc_reg(node.inputs[1], 0x25, node.ty != IrType::Int)
                        // JIT rx27 wave 8: or from its home word in place.
                        && !self.emit_alu_acc_frame(node.inputs[1], 0x25, node.ty != IrType::Int)
                    {
                        self.gp_load_value(RCX, node.inputs[1]);
                        if node.ty == IrType::Int {
                            // AND EAX, ECX
                            self.buf.emit(&[0x21, 0xC8]);
                        } else {
                            // AND RAX, RCX
                            self.buf.emit(&[0x48, 0x21, 0xC8]);
                        }
                        // `AND RCX, RAX` for an RCX carry (`RcxTwin`).
                        self.offer_alu_rcx_twin();
                    }
                }
                self.store_rax(slot);
            }
            Op::Or => {
                let slot = self.alloc_slot(id);
                // W9-2 (`CRATONVM_JIT_IR_REG_ALLOC`): straight into its register.
                if self.reg_alloc_direct_alu(id, slot) {
                    return;
                }
                self.gp_load_value(RAX, node.inputs[0]);
                if !self.emit_alu_acc_imm(node.inputs[1], 0x0D, node.ty != IrType::Int) {
                    // Round 17: or read in place from its register (`emit_alu_acc_reg`).
                    if !self.emit_alu_acc_reg(node.inputs[1], 0x0D, node.ty != IrType::Int)
                        // JIT rx27 wave 8: or from its home word in place.
                        && !self.emit_alu_acc_frame(node.inputs[1], 0x0D, node.ty != IrType::Int)
                    {
                        self.gp_load_value(RCX, node.inputs[1]);
                        if node.ty == IrType::Int {
                            // OR EAX, ECX
                            self.buf.emit(&[0x09, 0xC8]);
                        } else {
                            // OR RAX, RCX
                            self.buf.emit(&[0x48, 0x09, 0xC8]);
                        }
                        // `OR RCX, RAX` for an RCX carry (`RcxTwin`).
                        self.offer_alu_rcx_twin();
                    }
                }
                self.store_rax(slot);
            }
            Op::Xor => {
                let slot = self.alloc_slot(id);
                // W9-2 (`CRATONVM_JIT_IR_REG_ALLOC`): straight into its register.
                if self.reg_alloc_direct_alu(id, slot) {
                    return;
                }
                self.gp_load_value(RAX, node.inputs[0]);
                if !self.emit_alu_acc_imm(node.inputs[1], 0x35, node.ty != IrType::Int) {
                    // Round 17: or read in place from its register (`emit_alu_acc_reg`).
                    if !self.emit_alu_acc_reg(node.inputs[1], 0x35, node.ty != IrType::Int)
                        // JIT rx27 wave 8: or from its home word in place.
                        && !self.emit_alu_acc_frame(node.inputs[1], 0x35, node.ty != IrType::Int)
                    {
                        self.gp_load_value(RCX, node.inputs[1]);
                        if node.ty == IrType::Int {
                            // XOR EAX, ECX
                            self.buf.emit(&[0x31, 0xC8]);
                        } else {
                            // XOR RAX, RCX
                            self.buf.emit(&[0x48, 0x31, 0xC8]);
                        }
                        // `XOR RCX, RAX` for an RCX carry (`RcxTwin`).
                        self.offer_alu_rcx_twin();
                    }
                }
                self.store_rax(slot);
            }
            Op::Shl => {
                let slot = self.alloc_slot(id);
                // W9-2 (`CRATONVM_JIT_IR_REG_ALLOC`): straight into its register.
                if self.reg_alloc_direct_alu(id, slot) {
                    return;
                }
                self.gp_load_value(RAX, node.inputs[0]);
                if !self.emit_shift_imm(node.inputs[1], 0xE0, node.ty != IrType::Int) {
                    self.gp_load_value(RCX, node.inputs[1]);
                    if node.ty == IrType::Int {
                        // SHL EAX, CL
                        self.buf.emit(&[0xD3, 0xE0]);
                    } else {
                        // SHL RAX, CL
                        self.buf.emit(&[0x48, 0xD3, 0xE0]);
                    }
                }
                // Round 9 wave 11 (irl11): offer the carry-register twin of a
                // folded shift of a resident value (`RcxTwin`). No text here
                // may name that register: see the fold-guard audit.
                self.offer_shift_rcx_twin(
                    node.inputs[0],
                    node.inputs[1],
                    0xE0,
                    node.ty != IrType::Int,
                );
                self.store_rax(slot);
            }
            Op::Shr => {
                let slot = self.alloc_slot(id);
                // W9-2 (`CRATONVM_JIT_IR_REG_ALLOC`): straight into its register.
                if self.reg_alloc_direct_alu(id, slot) {
                    return;
                }
                self.gp_load_value(RAX, node.inputs[0]);
                if !self.emit_shift_imm(node.inputs[1], 0xF8, node.ty != IrType::Int) {
                    self.gp_load_value(RCX, node.inputs[1]);
                    if node.ty == IrType::Int {
                        // SAR EAX, CL
                        self.buf.emit(&[0xD3, 0xF8]);
                    } else {
                        // SAR RAX, CL
                        self.buf.emit(&[0x48, 0xD3, 0xF8]);
                    }
                }
                // Round 9 wave 11 (irl11): offer the carry-register twin of a
                // folded shift of a resident value (`RcxTwin`). No text here
                // may name that register: see the fold-guard audit.
                self.offer_shift_rcx_twin(
                    node.inputs[0],
                    node.inputs[1],
                    0xF8,
                    node.ty != IrType::Int,
                );
                self.store_rax(slot);
            }
            Op::UShr => {
                let slot = self.alloc_slot(id);
                // W9-2 (`CRATONVM_JIT_IR_REG_ALLOC`): straight into its register.
                if self.reg_alloc_direct_alu(id, slot) {
                    return;
                }
                self.gp_load_value(RAX, node.inputs[0]);
                if !self.emit_shift_imm(node.inputs[1], 0xE8, node.ty != IrType::Int) {
                    self.gp_load_value(RCX, node.inputs[1]);
                    if node.ty == IrType::Int {
                        // SHR EAX, CL
                        self.buf.emit(&[0xD3, 0xE8]);
                    } else {
                        // SHR RAX, CL
                        self.buf.emit(&[0x48, 0xD3, 0xE8]);
                    }
                }
                // Round 9 wave 11 (irl11): offer the carry-register twin of a
                // folded shift of a resident value (`RcxTwin`). No text here
                // may name that register: see the fold-guard audit.
                self.offer_shift_rcx_twin(
                    node.inputs[0],
                    node.inputs[1],
                    0xE8,
                    node.ty != IrType::Int,
                );
                self.store_rax(slot);
            }
            Op::Cmp(cc) => {
                // Fused into its consuming `Op::If` (see `lower_terminator`):
                // nothing to materialise here.
                if self.fused_cmp.get(id as usize).copied().unwrap_or(false) {
                    return;
                }
                let slot = self.alloc_slot(id);
                if self.emit_selected_materialized_cmp(id, slot) {
                    return;
                }
                self.gp_load_value(RAX, node.inputs[0]);
                // JIT rx27 wave 8: the second operand in place (the width rule
                // below, computed first).
                let wide_cmp = matches!(
                    self.graph.nodes[node.inputs[0] as usize].ty,
                    IrType::Ref | IrType::Long
                ) || matches!(
                    self.graph.nodes[node.inputs[1] as usize].ty,
                    IrType::Ref | IrType::Long
                );
                let in_place = self.emit_cmp_rax_with(node.inputs[1], wide_cmp);
                if !in_place {
                    self.gp_load_value(RCX, node.inputs[1]);
                }
                // CMP EAX, ECX — 32-bit for the int comparisons this node was
                // introduced for. A REFERENCE comparison (`ifnull`,
                // `if_acmpeq`) must compare all 64 bits: a heap pointer whose
                // low word happens to be zero would otherwise test equal to
                // null, and two distinct objects 4 GiB apart would test equal
                // to each other. Selected from the operand types, so an int
                // compare keeps the shorter encoding.
                //
                // A `long` compare needs all 64 bits for the same reason: the
                // builder's `ldiv`/`lrem` zero guard is `Cmp(Ne, divisor, 0L)`,
                // and a 32-bit CMP saw a divisor of `1L << 32` as zero and
                // deoptimised on every call.
                let ref_cmp = matches!(
                    self.graph.nodes[node.inputs[0] as usize].ty,
                    IrType::Ref | IrType::Long
                ) || matches!(
                    self.graph.nodes[node.inputs[1] as usize].ty,
                    IrType::Ref | IrType::Long
                );
                if in_place {
                    // Flags already set by `emit_cmp_rax_with`.
                } else if ref_cmp {
                    // CMP RAX, RCX
                    self.buf.emit(&[0x48, 0x39, 0xC8]);
                } else {
                    self.buf.emit(&[0x39, 0xC8]);
                }
                // SETcc AL — three bytes: `0F 9x C0`.
                //
                // BUG FIX [jit-irlower #1]: `x64_cc()` returns the *near-Jcc*
                // second byte (0x84..=0x8F, i.e. `0F 8x`). The SETcc second
                // byte is the Jcc value PLUS 0x10 (0x94..=0x9F, i.e. `0F 9x`),
                // NOT minus. The old `- 0x10` produced `0F 7x` (MMX
                // PCMPEQB/etc.), which never sets AL. Use `+ 0x10`.
                //
                // BUG FIX [jit-irlower #3]: the ModRM byte (`0xC0`, selecting
                // AL) was MISSING — only `0F 9x` was emitted. SETcc is a
                // /digit form and REQUIRES a ModRM operand byte; without it the
                // instruction stream desynced (the following `0F B6` MOVZX got
                // partly consumed as SETcc's ModRM) and AL/RAX were never
                // written, so RAX kept the first operand loaded above. That is
                // why `Op::Cmp` returned an input (`a`) instead of the 0/1
                // boolean, and why a Cmp feeding an `If` branched on `a` and
                // always took the else edge. Emit the full `0F 9x C0`.
                self.buf.emit(&[0x0F, cc.x64_cc() + 0x10, 0xC0]); // SETcc AL
                                                                  // MOVZX EAX, AL
                self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                self.store_rax(slot);
            }
            // An unboxing accessor, inline. The shape is the single-pass
            // backend's arm for the same two triples, node for node, and it has
            // to be: both derive their offsets from `ir::unbox_offsets`, so if
            // the sequences drifted apart one of them would be reading the
            // wrong half of an object.
            //
            // NULL and the class guard DEOPT rather than throw. `Long` and
            // `Integer` are final, but the guard is not therefore pointless:
            // the receiver here is whatever the constant pool said, and a site
            // that sees a different class must run the real dispatch. On the
            // null path the interpreter re-runs the call and raises the NPE
            // that `longValue()` on `null` raises today.
            //
            // The LAYOUT BRANCH is the reason this family was not lowered
            // earlier: a compact instance keeps the payload at a registered
            // body offset, a legacy one inside its 16-byte `Value` cell, and
            // both exist at once because different allocators build different
            // cells. There is no compile-time answer -- the header bit is read
            // per object, every time.
            Op::Unbox { op, class_id } => {
                let uop = *op;
                let class_id = *class_id;
                let bci = self.deopt_pc_of(node);
                // JIT round 23 close (`CRATONVM_JIT_IR_FFM_SESSION_PRECHECK`):
                // the loop-entry session check, which answers and never deopts.
                if uop == crate::ir::UnboxOp::SessionPrecheck {
                    let slot = self.alloc_slot(id);
                    self.lower_session_precheck(node, slot);
                    return;
                }
                // JIT round 24 B2 (`CRATONVM_JIT_IR_FFM_ARENA_CACHE`): the
                // loop-entry arena slot and address, which answer and never
                // deopt.
                if uop == crate::ir::UnboxOp::ArenaCacheEntry {
                    let slot = self.alloc_slot(id);
                    self.lower_arena_cache_entry(node, slot);
                    return;
                }
                if uop == crate::ir::UnboxOp::ArenaCacheBase {
                    let slot = self.alloc_slot(id);
                    self.lower_arena_cache_base(node, slot);
                    return;
                }
                if uop.is_scoped() {
                    let slot = self.alloc_slot(id);
                    // JIT r23 wave 8 (lane W8-L4): `Graph::ffm_proven_heap_accesses`.
                    let proven = self.graph.ffm_proven_heap_accesses.contains(&id);
                    // JIT round 24 B2: `Graph::ffm_arena_cached`.
                    let cache = crate::ir_optimize::ffm_arena_cache::cache_inputs(self.graph, id);
                    self.lower_scoped_raw_cached(node, uop, bci, slot, proven, cache);
                    return;
                }
                let Some((compact_off, legacy_off)) = crate::ir::unbox_offsets(uop, class_id)
                else {
                    // The planner already asked and got an answer; a refusal
                    // here means the layout moved under us between planning and
                    // lowering. Refuse the artifact rather than emit a load
                    // from an offset nobody vouched for.
                    self.latch_bailout(Bailout::new(BailoutReason::Internal(
                        "ir_lower: unbox intrinsic layout no longer resolves",
                    )));
                    return;
                };
                // JIT r23 wave 4 stage 1 (lane L8): a reference read honours
                // the collector's read barrier the way every inline reference
                // load does -- refused while ZGC's is armed, and needing the
                // `JIT_READ_BOUNDS` table whose liveness the emitted read
                // checks (`emit_ffm_element_read_bounds_live_deopt`).
                if matches!(uop, crate::ir::UnboxOp::FieldRef(_))
                    && (crate::x64::zgc_read_barrier_blocks_inline_fields()
                        || self.read_bounds_addr == 0)
                {
                    self.latch_bailout(Bailout::new(BailoutReason::Internal(
                        "ir_lower: reference field read under an armed read barrier or without JIT_READ_BOUNDS",
                    )));
                    return;
                }
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[2]); // receiver

                let recv = node.inputs[2];
                let unbox_cse = crate::ir::ir_unbox_guard_cse_enabled();
                let class_proven = self.unbox_exact_class_proven(recv, class_id);
                let exact_if_non_null = !class_proven
                    && (self.checkcast_null_or_exact_proven.contains(&(recv, class_id))
                        || self
                            .checkcast_null_or_exact_proven
                            .contains(&(self.field_proof_canon(recv), class_id)));
                let non_null_proven = class_proven || self.receiver_already_non_null(recv);
                if !non_null_proven {
                    // null -> deopt
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
                    self.note_receiver_non_null(recv);
                }

                // Tier proposal 27 phase 3 (round 11 wave 12, lane irexc): for
                // the FINAL `Integer`/`Long` the site's receiver is one (the
                // constant pool names the class, and the verifier types the
                // receiver as it), so a failure of either guard below is a
                // layout event, and the interpreter's `intValue()` /
                // `longValue()` returns the payload this load would have. Not
                // so for the `Atomic*` families: a subclass can override.
                let value_equivalent = matches!(
                    uop,
                    crate::ir::UnboxOp::IntValue | crate::ir::UnboxOp::LongValue
                );

                if !class_proven && !exact_if_non_null {
                    // exact receiver class guard: CMP DWORD [RAX+0], class_id
                    self.buf.emit(&[0x81, 0x78, 0x00]);
                    self.buf.emit(&class_id.to_le_bytes());
                    self.emit_value_equivalent_deopt_unless(
                        0x84, // JE continue
                        bci,
                        DeoptReason::ReceiverTypeChanged,
                        value_equivalent,
                    );
                    if unbox_cse {
                        self.unbox_class_proven.push((recv, class_id));
                    }
                } else if exact_if_non_null && unbox_cse {
                    self.unbox_class_proven.push((recv, class_id));
                }

                // Both offsets below were resolved at compile time; a layout
                // REPLACEMENT of the boxed class would leave them stale (and the
                // `LOCK XADD` forms would add into the wrong word). RAX survives.
                if !self.unbox_layout_epoch_proven() && self.emit_layout_epoch_cmp() {
                    self.emit_value_equivalent_deopt_unless(
                        0x84, // JE continue
                        bci,
                        DeoptReason::ReceiverTypeChanged,
                        value_equivalent,
                    );
                    self.block_cse_mark_layout_epoch_checked();
                }
                if matches!(uop, crate::ir::UnboxOp::FieldRef(_)) {
                    self.emit_ffm_element_read_bounds_live_deopt(bci);
                }

                let wide = uop.is_wide();
                self.emit_unbox_delta_and_field_access(
                    node,
                    uop,
                    recv,
                    class_id,
                    compact_off,
                    legacy_off,
                    bci,
                );
                let _ = wide;
                self.finish_unbox_result(id, uop, slot);
            }
            // A call-site intrinsic lowered as arithmetic rather than as a
            // call. See `ir::Op::ScalarIntrinsic` for why this exists and why
            // it is one op with a sub-enum.
            //
            // WIDTH CONVENTION, and it is the thing to get right here: every
            // consumer of an `Int` slot in this backend reads only its LOW 32
            // bits. The upper half is not uniform across producers -- a 32-bit
            // ALU result (`Op::Add`'s `ADD EAX, ECX; store_rax`) zeroes it,
            // while int `ArrayLoad`, `F2I`, `LCmp` and a negative `Const`
            // sign-extend -- which is exactly why an int consumer must not read
            // it (see the bitwise arms and `emit_array_null_bounds_guards_for`).
            // So the 32-bit families leave their answer in EAX and do not
            // sign-extend. `CompareL` is the exception that proves the rule
            // rather than an inconsistency: it takes LONG operands and returns
            // an `int`, so its `CMP` is 64-bit while its result is not -- which
            // is exactly why `ScalarOp::operands_are_long` exists separately
            // from `ScalarOp::result_type`. Sizing the compare from the result
            // type would compare the low halves of two longs.
            Op::ScalarIntrinsic(sop) => {
                let sop = *sop;
                // A site the compile door found unresolved resolves at its
                // first execution even when it is arithmetic
                // (`ir::scalar_intrinsic_first_call_enabled`): the same
                // register-transparent leaf guard `Op::Call` emits.
                if let Some(&(_, _, cell)) = self
                    .graph
                    .scalar_first_call_cells
                    .iter()
                    .find(|&&(pc, op, _)| Some(pc) == node.bytecode_pc && op == sop)
                {
                    crate::runtime_lowering::emit_first_call_guard(&mut self.buf, cell);
                }
                let slot = self.alloc_slot(id);
                let w = sop.operands_are_long();
                if sop.is_fp() {
                    // The FP families live in XMM and never touch RAX. Handled
                    // before the general-purpose load below, which would
                    // otherwise read an FP value through `gp_load_value` and
                    // hand the arm a bit pattern in the wrong register file.
                    //
                    // `Math.abs` is one AND against a sign mask. The mask goes
                    // via a GP register because this tier has no FP constant
                    // pool; XMM1 is already the designated mask register (see
                    // the file header's XMM role note).
                    if let ScalarOp::MathCall(m) = sop {
                        self.lower_fp_math_c_call(id, slot, m.fn_ptr(), m.arity(), node.inputs.as_slice());
                        return;
                    }
                    if matches!(sop, ScalarOp::FmaD | ScalarOp::FmaF) {
                        // JIT round 19 wave 2 (lane irops2, IROPS19-2).
                        self.lower_fp_fma_scalar(id, slot, sop, node.inputs.as_slice());
                        return;
                    }
                    if !matches!(sop, ScalarOp::AbsF | ScalarOp::AbsD) {
                        // JIT round 19 (lane irops19, R15B-31): sqrt and the
                        // FP min/max families. Own function so this arm keeps
                        // its one `fp_store_value` shape.
                        let b = node.inputs.get(1).copied();
                        self.lower_fp_math_scalar(id, slot, sop, node.inputs[0], b);
                        return;
                    }
                    let dbl = matches!(sop, ScalarOp::AbsD);
                    self.fp_load_value(XMM0, node.inputs[0], dbl);
                    if dbl {
                        self.emit_mov_reg_imm64(RAX, 0x7FFF_FFFF_FFFF_FFFF);
                        self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC8]); // MOVQ XMM1, RAX
                        self.buf.emit(&[0x66, 0x0F, 0x54, 0xC1]); // ANDPD XMM0, XMM1
                    } else {
                        self.buf.emit(&[0xB8]); // MOV EAX, imm32
                        self.buf.emit(&0x7FFF_FFFFu32.to_le_bytes());
                        self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC8]); // MOVD XMM1, EAX
                        self.buf.emit(&[0x0F, 0x54, 0xC1]); // ANDPS XMM0, XMM1
                    }
                    self.fp_store_value(id, slot, XMM0, dbl);
                    return;
                }
                // Round 15 wave 6 (lane irosr2, IRO-1): the raw FP-bits
                // families, the only CROSS-BANK members. One bitwise move each
                // (`MOVQ`/`MOVD`: no conversion, so a NaN keeps its payload and
                // a signalling NaN stays signalling), the single-pass FP_BITS
                // intrinsics' semantics. An XMM result leaves through
                // `fp_store_value` and returns here; a general-purpose one is
                // left in RAX for the one `store_rax` at the bottom of the arm
                // (`op_home_is_one_store_rax` claims this arm).
                let raw_bits = sop.raw_bits_move();
                if let Some((true, dbl)) = raw_bits {
                    self.gp_load_value(RAX, node.inputs[0]);
                    if dbl {
                        self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // MOVQ XMM0, RAX
                    } else {
                        self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
                    }
                    self.fp_store_value(id, slot, XMM0, dbl);
                    return;
                }
                if let Some((false, dbl)) = raw_bits {
                    self.fp_load_value(XMM0, node.inputs[0], dbl);
                    if dbl {
                        self.buf.emit(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]); // MOVQ RAX, XMM0
                    } else {
                        // MOVD EAX, XMM0: the upper half of RAX is zero, the
                        // `Int` convention this arm's other 32-bit families keep.
                        self.buf.emit(&[0x66, 0x0F, 0x7E, 0xC0]);
                    }
                } else if let Some(dbl) = sop.round_fp_input() {
                    // JIT r22 wave 1, lane intrinsics (INTRINSICS21-7):
                    // `Math.round` keeps the argument in XMM0 for its
                    // saturating fallback and reads its raw bits in RAX.
                    self.fp_load_value(XMM0, node.inputs[0], dbl);
                    if dbl {
                        self.buf.emit(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]); // MOVQ RAX, XMM0
                    } else {
                        self.buf.emit(&[0x66, 0x0F, 0x7E, 0xC0]); // MOVD EAX, XMM0
                    }
                } else {
                    self.gp_load_value(RAX, node.inputs[0]); // a
                }
                match sop {
                    ScalarOp::MinI | ScalarOp::MaxI | ScalarOp::MinL | ScalarOp::MaxL => {
                        self.gp_load_value(RCX, node.inputs[1]); // b
                        if w {
                            self.buf.emit(&[0x48, 0x39, 0xC8]); // CMP RAX, RCX
                        } else {
                            self.buf.emit(&[0x39, 0xC8]); // CMP EAX, ECX
                        }
                        // MIN takes b when a > b (CMOVG); MAX takes b when
                        // a < b (CMOVL). Both are signed, which is what
                        // `Math.min`/`max` on `int`/`long` specify.
                        let cc = if matches!(sop, ScalarOp::MinI | ScalarOp::MinL) {
                            0x4F // CMOVG
                        } else {
                            0x4C // CMOVL
                        };
                        // CMOVcc EAX, ECX -- ModRM C1 (mod=11 reg=EAX rm=ECX).
                        if w {
                            self.buf.emit(&[0x48, 0x0F, cc, 0xC1]);
                        } else {
                            self.buf.emit(&[0x0F, cc, 0xC1]);
                        }
                    }
                    ScalarOp::AbsI => {
                        // Branchless: t = x >> 31 (arithmetic); (x ^ t) - t.
                        // At `Integer.MIN_VALUE` this yields MIN_VALUE, which
                        // is what JLS 15.15.4 specifies -- the negation wraps.
                        // A `CMOV` form written from `x < 0 ? -x : x` agrees
                        // only because its negation wraps too; this one is
                        // shorter and has no flag dependency to schedule
                        // around.
                        self.buf.emit(&[0x89, 0xC2]); // MOV EDX, EAX
                        self.buf.emit(&[0xC1, 0xFA, 0x1F]); // SAR EDX, 31
                        self.buf.emit(&[0x31, 0xD0]); // XOR EAX, EDX
                        self.buf.emit(&[0x29, 0xD0]); // SUB EAX, EDX
                    }
                    ScalarOp::AbsL => {
                        self.buf.emit(&[0x48, 0x89, 0xC2]); // MOV RDX, RAX
                        self.buf.emit(&[0x48, 0xC1, 0xFA, 0x3F]); // SAR RDX, 63
                        self.buf.emit(&[0x48, 0x31, 0xD0]); // XOR RAX, RDX
                        self.buf.emit(&[0x48, 0x29, 0xD0]); // SUB RAX, RDX
                    }
                    // -- Bit-scan families ------------------------------
                    //
                    // BSR and BSF leave the DESTINATION UNDEFINED when the
                    // source is zero, and set ZF. So each sequence below reads
                    // ZF before anything clobbers the flags; the `MOV` that
                    // sits between the scan and the `CMOVZ`/`SETZ` is chosen
                    // because MOV does not touch flags, and swapping it for
                    // anything that does is a wrong answer on zero only.
                    // JIT round 19 wave 2 (lane irops2, IROPS19-2): one
                    // `LZCNT` (defined at zero: the operand width) where the
                    // host has it and `CRATONVM_JIT_IR_LZCNT_TZCNT` is on.
                    // The gate matters for correctness, not only speed: on a
                    // host without LZCNT the same bytes decode as `BSR`.
                    ScalarOp::NlzI | ScalarOp::NlzL if crate::x64::has_lzcnt() => {
                        if w {
                            self.buf.emit(&[0xF3, 0x48, 0x0F, 0xBD, 0xC0]); // LZCNT RAX, RAX
                        } else {
                            self.buf.emit(&[0xF3, 0x0F, 0xBD, 0xC0]); // LZCNT EAX, EAX
                        }
                    }
                    // `TZCNT` (BMI1), likewise; it decodes as `BSF` without BMI1.
                    ScalarOp::NtzI | ScalarOp::NtzL if crate::x64::has_bmi1() => {
                        if w {
                            self.buf.emit(&[0xF3, 0x48, 0x0F, 0xBC, 0xC0]); // TZCNT RAX, RAX
                        } else {
                            self.buf.emit(&[0xF3, 0x0F, 0xBC, 0xC0]); // TZCNT EAX, EAX
                        }
                    }
                    // JIT round 19 wave 2 (lane irops2, IROPS19-2):
                    // `Integer`/`Long.bitCount`, one `POPCNT` (the recognizer
                    // admitted the row only with POPCNT present). The F3
                    // prefix precedes REX. The `int` result is in 0..=64, so
                    // its zero-extended upper half is also its sign extension.
                    ScalarOp::BitCountI | ScalarOp::BitCountL => {
                        if w {
                            self.buf.emit(&[0xF3, 0x48, 0x0F, 0xB8, 0xC0]); // POPCNT RAX, RAX
                        } else {
                            self.buf.emit(&[0xF3, 0x0F, 0xB8, 0xC0]); // POPCNT EAX, EAX
                        }
                    }
                    ScalarOp::NlzI | ScalarOp::NlzL => {
                        // BSR gives the index of the highest set bit, so
                        // nlz = (width-1) - index. The zero case is folded in
                        // by moving the index to -1 first, which makes
                        // (width-1) - (-1) = width without a branch.
                        if w {
                            self.buf.emit(&[0x48, 0x0F, 0xBD, 0xC8]); // BSR RCX, RAX
                        } else {
                            self.buf.emit(&[0x0F, 0xBD, 0xC8]); // BSR ECX, EAX
                        }
                        self.buf.emit(&[0xBA]); // MOV EDX, imm32 (flags untouched)
                        self.buf.emit(&(-1i32).to_le_bytes());
                        self.buf.emit(&[0x0F, 0x44, 0xCA]); // CMOVZ ECX, EDX
                        self.buf.emit(&[0xB8]); // MOV EAX, imm32
                        self.buf
                            .emit(&(if w { 63i32 } else { 31i32 }).to_le_bytes());
                        self.buf.emit(&[0x29, 0xC8]); // SUB EAX, ECX
                    }
                    ScalarOp::NtzI | ScalarOp::NtzL => {
                        // BSF gives the index of the lowest set bit, which IS
                        // the answer; only the zero case needs the width.
                        if w {
                            self.buf.emit(&[0x48, 0x0F, 0xBC, 0xC8]); // BSF RCX, RAX
                        } else {
                            self.buf.emit(&[0x0F, 0xBC, 0xC8]); // BSF ECX, EAX
                        }
                        self.buf.emit(&[0xBA]); // MOV EDX, imm32
                        self.buf
                            .emit(&(if w { 64i32 } else { 32i32 }).to_le_bytes());
                        self.buf.emit(&[0x0F, 0x44, 0xCA]); // CMOVZ ECX, EDX
                        self.buf.emit(&[0x89, 0xC8]); // MOV EAX, ECX
                    }
                    ScalarOp::ReverseBytesI | ScalarOp::ReverseBytesL => {
                        if w {
                            self.buf.emit(&[0x48, 0x0F, 0xC8]); // BSWAP RAX
                        } else {
                            self.buf.emit(&[0x0F, 0xC8]); // BSWAP EAX
                        }
                    }
                    ScalarOp::LowestOneBitI | ScalarOp::LowestOneBitL => {
                        // x & -x. No scan, so no undefined register and no
                        // zero edge: -0 is 0 and 0 & 0 is 0.
                        if w {
                            self.buf.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
                            self.buf.emit(&[0x48, 0xF7, 0xD9]); // NEG RCX
                            self.buf.emit(&[0x48, 0x21, 0xC8]); // AND RAX, RCX
                        } else {
                            self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX
                            self.buf.emit(&[0xF7, 0xD9]); // NEG ECX
                            self.buf.emit(&[0x21, 0xC8]); // AND EAX, ECX
                        }
                    }
                    ScalarOp::HighestOneBitI | ScalarOp::HighestOneBitL => {
                        // 1 << bsr(x), and 0 when x is 0. The shift CLOBBERS
                        // the flags, so ZF is captured into DL by SETZ first
                        // and turned into an all-ones/all-zero mask; the shift
                        // itself runs on an undefined count in the zero case
                        // and its garbage is masked away.
                        if w {
                            self.buf.emit(&[0x48, 0x0F, 0xBD, 0xC8]); // BSR RCX, RAX
                        } else {
                            self.buf.emit(&[0x0F, 0xBD, 0xC8]); // BSR ECX, EAX
                        }
                        self.buf.emit(&[0x0F, 0x94, 0xC2]); // SETZ DL
                        self.buf.emit(&[0xB8]); // MOV EAX, 1
                        self.buf.emit(&1i32.to_le_bytes());
                        if w {
                            self.buf.emit(&[0x48, 0xD3, 0xE0]); // SHL RAX, CL
                        } else {
                            self.buf.emit(&[0xD3, 0xE0]); // SHL EAX, CL
                        }
                        self.buf.emit(&[0x0F, 0xB6, 0xD2]); // MOVZX EDX, DL
                        self.buf.emit(&[0xFF, 0xCA]); // DEC EDX  (0 -> -1, 1 -> 0)
                        if w {
                            self.buf.emit(&[0x48, 0x63, 0xD2]); // MOVSXD RDX, EDX
                            self.buf.emit(&[0x48, 0x21, 0xD0]); // AND RAX, RDX
                        } else {
                            self.buf.emit(&[0x21, 0xD0]); // AND EAX, EDX
                        }
                    }
                    ScalarOp::RotateLeftI
                    | ScalarOp::RotateLeftL
                    | ScalarOp::RotateRightI
                    | ScalarOp::RotateRightL => {
                        // The DISTANCE is an `int` at both widths -- see
                        // `ScalarOp::input_ty`. x86 masks CL to 5 bits for a
                        // 32-bit rotate and 6 for a 64-bit one, which is
                        // exactly the `distance & 31` / `& 63` the JLS
                        // specifies, negative distances included.
                        self.gp_load_value(RCX, node.inputs[1]);
                        let left = matches!(sop, ScalarOp::RotateLeftI | ScalarOp::RotateLeftL);
                        let modrm = if left { 0xC0 } else { 0xC8 };
                        if w {
                            self.buf.emit(&[0x48, 0xD3, modrm]); // ROL/ROR RAX, CL
                        } else {
                            self.buf.emit(&[0xD3, modrm]); // ROL/ROR EAX, CL
                        }
                    }
                    // The value is already in RAX (the cross-bank load above).
                    ScalarOp::DoubleToRawLongBits | ScalarOp::FloatToRawIntBits => {}
                    ScalarOp::MathCall(_) => unreachable!(),
                    // JIT round 19 (lane irops19, R15B-31): the high half of
                    // the 128-bit product. One-operand `IMUL r64` (signed) /
                    // `MUL r64` (unsigned) leave RDX:RAX = RAX * RCX; RDX is
                    // already this arm's scratch (`AbsI`, the bit scans), and
                    // the product is commutative, so operand order is moot.
                    ScalarOp::MultiplyHighL | ScalarOp::UnsignedMultiplyHighL => {
                        self.gp_load_value(RCX, node.inputs[1]); // b
                        let modrm = if matches!(sop, ScalarOp::MultiplyHighL) {
                            0xE9 // /5 IMUL RCX
                        } else {
                            0xE1 // /4 MUL RCX
                        };
                        self.buf.emit(&[0x48, 0xF7, modrm]);
                        self.buf.emit(&[0x48, 0x89, 0xD0]); // MOV RAX, RDX
                    }
                    // nio20 wave 6, lane L3: `lo <= ch < lo + 0x400` as ONE
                    // unsigned compare of `ch - lo` -- exact for any 32-bit
                    // operand (a value below `lo`, negative included, wraps
                    // above 0x400). The answer is 0/1 in EAX, upper half zero.
                    ScalarOp::IsHighSurrogate | ScalarOp::IsLowSurrogate => {
                        let lo: u32 =
                            if sop == ScalarOp::IsHighSurrogate { 0xD800 } else { 0xDC00 };
                        self.buf.emit(&[0x2D]); // SUB EAX, imm32
                        self.buf.emit(&lo.to_le_bytes());
                        self.buf.emit(&[0x3D]); // CMP EAX, imm32
                        self.buf.emit(&0x400u32.to_le_bytes());
                        self.buf.emit(&[0x0F, 0x92, 0xC0]); // SETB AL
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                    }
                    // JIT round 20 wave 11, lane pIR (L5W1-2): the code-point
                    // tests, one 32-bit compare each. `charCount` is the JDK's
                    // SIGNED `cp >= 0x10000` (SETGE, then +1: 1 or 2); the two
                    // predicates are `cp >>> 16 == 0` / `cp >>> 16 < 0x11`,
                    // i.e. an UNSIGNED `cp < 0x10000` / `cp < 0x110000`
                    // (SETB), exact for every operand, negative included.
                    ScalarOp::CharCount => {
                        self.buf.emit(&[0x3D]); // CMP EAX, imm32
                        self.buf.emit(&0x1_0000u32.to_le_bytes());
                        self.buf.emit(&[0x0F, 0x9D, 0xC0]); // SETGE AL
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                        self.buf.emit(&[0xFF, 0xC0]); // INC EAX
                    }
                    ScalarOp::IsBmpCodePoint | ScalarOp::IsValidCodePoint => {
                        let bound: u32 =
                            if sop == ScalarOp::IsBmpCodePoint { 0x1_0000 } else { 0x11_0000 };
                        self.buf.emit(&[0x3D]); // CMP EAX, imm32
                        self.buf.emit(&bound.to_le_bytes());
                        self.buf.emit(&[0x0F, 0x92, 0xC0]); // SETB AL
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                    }
                    ScalarOp::IsIsoControl => {
                        self.buf.emit(&[0x83, 0xF8, 0x1F]); // CMP EAX, 0x1F
                        self.buf.emit(&[0x0F, 0x96, 0xC1]); // SETBE CL
                        self.buf.emit(&[0x83, 0xE8, 0x7F]); // SUB EAX, 0x7F
                        self.buf.emit(&[0x83, 0xF8, 0x20]); // CMP EAX, 0x20
                        self.buf.emit(&[0x0F, 0x96, 0xC0]); // SETBE AL
                        self.buf.emit(&[0x08, 0xC8]);       // OR AL, CL
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                    }
                    // JIT round 22 wave 1, lane intrinsics (INTRINSICS21-7):
                    // XMM0 = the argument, RAX = its raw bits (the load
                    // above); the result is left in RAX.
                    ScalarOp::RoundD | ScalarOp::RoundF => {
                        self.emit_math_round_scalar(sop == ScalarOp::RoundD);
                    }
                    // j27 wave 6 (lane T1BCE): BMI2 `PEXT` / `PDEP` RAX, RAX
                    // (value, loaded above), RCX (mask); the 32-bit forms
                    // zero the upper half. The row is recognised only on a
                    // BMI2 host; a node reaching here without one is refused
                    // (the bytes would #UD), never emitted.
                    ScalarOp::CompressI
                    | ScalarOp::CompressL
                    | ScalarOp::ExpandI
                    | ScalarOp::ExpandL => {
                        let bytes = crate::ir::bit_compress::pext_pdep_rax_rax_rcx(sop);
                        let Some(bytes) = bytes.filter(|_| crate::x64::has_bmi2()) else {
                            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                                "ir_lower: a bit compress / expand row without BMI2",
                            )));
                            return;
                        };
                        self.gp_load_value(RCX, node.inputs[1]); // mask
                        self.buf.emit(&bytes);
                    }
                    ScalarOp::AbsF
                    | ScalarOp::AbsD
                    | ScalarOp::SqrtD
                    | ScalarOp::MinF
                    | ScalarOp::MaxF
                    | ScalarOp::MinD
                    | ScalarOp::MaxD
                    | ScalarOp::FloorD
                    | ScalarOp::CeilD
                    | ScalarOp::RintD
                    | ScalarOp::FmaD
                    | ScalarOp::FmaF
                    | ScalarOp::LongBitsToDouble
                    | ScalarOp::IntBitsToFloat => {
                        // Unreachable: the `is_fp()` guard above (or, for the
                        // two XMM-result raw-bits families, the `raw_bits`
                        // guard) returns before this match. Spelled out rather than folded into a
                        // catch-all so that adding a THIRD FP family without
                        // extending that guard is a compile error here, not a
                        // value silently read out of RAX. Refused rather than
                        // panicked on (round 11 wave 3): a compile thread must
                        // not die on a broken invariant; the method keeps its
                        // single-pass body.
                        self.latch_bailout(Bailout::new(BailoutReason::Internal(
                            "ir_lower: an FP scalar intrinsic reached the integer arm",
                        )));
                        return;
                    }
                    ScalarOp::CompareI | ScalarOp::CompareL => {
                        // Byte for byte the `Op::LCmp` sequence below, which is
                        // `Long.compare`'s specification already: SETG minus
                        // SETL is exactly {-1, 0, 1}.
                        self.gp_load_value(RCX, node.inputs[1]); // b
                        if w {
                            self.buf.emit(&[0x48, 0x39, 0xC8]); // CMP RAX, RCX
                        } else {
                            self.buf.emit(&[0x39, 0xC8]); // CMP EAX, ECX
                        }
                        self.buf.emit(&[0x0F, 0x9F, 0xC0]); // SETG AL
                        self.buf.emit(&[0x0F, 0x9C, 0xC2]); // SETL DL
                        self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                        self.buf.emit(&[0x0F, 0xB6, 0xD2]); // MOVZX EDX, DL
                        self.buf.emit(&[0x29, 0xD0]); // SUB EAX, EDX
                    }
                }
                self.store_rax(slot);
            }
            // inc 27: `lcmp` 3-way signed compare of two longs → int {-1,0,1}.
            // result = (a > b) − (a < b), using signed SETcc on a 64-bit CMP, then
            // sign-extended to 64 bits so a 32- or 64-bit consumer both read it
            // correctly (the typical consumer is an `if<cond>` against 0).
            Op::LCmp => {
                // Fused into the compare-and-branch of the `If` that consumes
                // it (see `fused_lcmp` and `lower_terminator`): nothing to
                // materialise here.
                if self.fused_lcmp.get(id as usize).copied().unwrap_or(false) {
                    return;
                }
                let slot = self.alloc_slot(id);
                if !self.try_allocated_lcmp(node.inputs[0], node.inputs[1]) {
                    self.gp_load_value(RAX, node.inputs[0]); // a
                    self.gp_load_value(RCX, node.inputs[1]); // b
                    self.buf.emit(&[0x48, 0x39, 0xC8]); // CMP RAX, RCX (signed, 64-bit)
                }
                self.buf.emit(&[0x0F, 0x9F, 0xC0]); // SETG AL  (a > b)
                self.buf.emit(&[0x0F, 0x9C, 0xC2]); // SETL DL  (a < b)
                self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                self.buf.emit(&[0x0F, 0xB6, 0xD2]); // MOVZX EDX, DL
                self.buf.emit(&[0x29, 0xD0]); // SUB EAX, EDX  → {-1,0,1}
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX (sign-extend)
                self.store_rax(slot);
            }
            Op::FCmp {
                double,
                nan_greater,
            } => {
                // FP 3-way compare → int {-1,0,1}, mirroring `Op::LCmp` but via
                // `ucomis` with the JVMS NaN-unordered rule. Branchless:
                // `result = AL - DL` where the operand order + SETcc choice put
                // a NaN operand on +1 (`cmpg`) or -1 (`cmpl`). `ucomis` raises
                // CF on BOTH "below" and "unordered", which is what makes the
                // NaN case fall out for free:
                //   cmpl: UCOMIS a,b ; AL=SETA(a>b) ; DL=SETB(a<b OR NaN)
                //         ⇒ AL-DL = {a>b:+1, a<b:-1, eq:0, NaN:-1}.
                //   cmpg: UCOMIS b,a ; AL=SETB((a>b) OR NaN) ; DL=SETA(a<b)
                //         ⇒ AL-DL = {a>b:+1, a<b:-1, eq:0, NaN:+1}.
                // Fused into the `UCOMIS; Jcc` of the `If` that consumes it
                // (see `fused_fcmp` and `lower_terminator`): nothing to
                // materialise here.
                if self.fused_fcmp.get(id as usize).copied().unwrap_or(false) {
                    return;
                }
                let is_d = *double;
                let slot = self.alloc_slot(id);
                self.fp_load_value(XMM0, node.inputs[0], is_d); // a
                self.fp_load_value(XMM1, node.inputs[1], is_d); // b
                if *nan_greater {
                    // UCOMIS XMM1, XMM0 (compare b vs a) — ModRM C8.
                    if is_d {
                        self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC8]);
                    } else {
                        self.buf.emit(&[0x0F, 0x2E, 0xC8]);
                    }
                    self.buf.emit(&[0x0F, 0x92, 0xC0]); // SETB AL  ((a>b) OR NaN)
                    self.buf.emit(&[0x0F, 0x97, 0xC2]); // SETA DL  (a<b)
                } else {
                    // UCOMIS XMM0, XMM1 (compare a vs b) — ModRM C1.
                    if is_d {
                        self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC1]);
                    } else {
                        self.buf.emit(&[0x0F, 0x2E, 0xC1]);
                    }
                    self.buf.emit(&[0x0F, 0x97, 0xC0]); // SETA AL  (a>b)
                    self.buf.emit(&[0x0F, 0x92, 0xC2]); // SETB DL  (a<b OR NaN)
                }
                self.buf.emit(&[0x0F, 0xB6, 0xC0]); // MOVZX EAX, AL
                self.buf.emit(&[0x0F, 0xB6, 0xD2]); // MOVZX EDX, DL
                self.buf.emit(&[0x29, 0xD0]); // SUB EAX, EDX  → {-1,0,1}
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX (sign-extend)
                self.store_rax(slot);
            }
            Op::I2L => {
                if self.fused_i2l.get(id as usize).copied().unwrap_or(false) {
                    return;
                }
                let slot = self.alloc_slot(id);
                if self.try_allocated_i2l(id, node.inputs[0], slot) {
                    return;
                }
                self.gp_load_value(RAX, node.inputs[0]);
                if !self.int_already_sign_extended(node.inputs[0]) {
                    // MOVSXD RAX, EAX
                    self.buf.emit(&[0x48, 0x63, 0xC0]);
                }
                self.store_rax(slot);
            }
            Op::L2I => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                // MOV EAX, EAX (zero-extend / truncate to 32 bits)
                self.buf.emit(&[0x89, 0xC0]);
                self.store_rax(slot);
            }
            Op::Phi => {
                // Phi nodes are resolved by predecessors storing their
                // incoming value into the phi's slot at the controlling
                // block edge. The slot is reserved up front by
                // `prealloc_phi_slots` (a forward branch may target a phi
                // whose block has not yet been lowered), and the parallel
                // copy is emitted by `emit_phi_copies` (see BUG FIX
                // [jit-irlower #2]) just before each predecessor's branch
                // to this phi's merge block. Nothing to emit here.
            }
            Op::Guard { bci } => {
                // real-frame-deopt step 3: a speculative guard. If `cond`
                // (inputs[1]) is zero, transfer to the shared deopt stub which
                // reconstructs the interpreter frame for `bci` and returns the
                // deopt sentinel; otherwise fall through.
                let bci = *bci;
                // JIT round 21 (lane charat, STRINTR21-5,
                // `CRATONVM_JIT_IR_GUARD_FACT_ELIDE`): a dominating fact proves
                // the condition (`ir_check_elim::guard_proven`), so the guard
                // can never fire: no test, no pad, no deopt point. A fused
                // compare emits nothing at its own position, so it goes too.
                // A guard armed with an implicit-throw pad keeps its arm.
                if self.check_elision.guard_elided(id)
                    && !self.implicit_throw_plans.contains_key(&id)
                {
                    crate::ir_evidence::note(crate::ir_evidence::Transform::GuardElided);
                    return;
                }
                // Round 11 (irlower): a `Cmp` whose only use is this guard is
                // FUSED into it (`prepare_fusion_tables`): no `SETcc; MOVZX;
                // store` for the boolean and no reload + `TEST` here -- the
                // guard branches to its pad on the compare's own flags. The
                // compare then has no home, so `slot_of` must not be asked.
                let fused: Option<(crate::ir::CmpOp, NodeId, NodeId)> =
                    match node.inputs.get(1).copied() {
                        Some(c) if self.fused_cmp.get(c as usize).copied().unwrap_or(false) => {
                            match self.graph.nodes.get(c as usize) {
                                Some(cn) if cn.inputs.len() >= 2 => match cn.op {
                                    Op::Cmp(cc) => Some((cc, cn.inputs[0], cn.inputs[1])),
                                    _ => None,
                                },
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                // JIT round 20 wave 5 (lane loopopt): a condition whose home
                // was DROPPED -- a constant (`Guard(Const(0))`, a site trap or
                // a guard that folded) under CG16P-4, or a register-resident
                // value -- has no home word; `slot_of` refused it and the whole
                // optimizing compile was lost (9 compiles in one
                // `Nio21LoopReload` run). Read it through `gp_load_value`,
                // which materialises a constant and reads a register.
                let cond_id = node.inputs[1];
                if !self.implicit_throw_plans.contains_key(&id)
                    && self.graph.nodes.get(cond_id as usize).is_some_and(|c| {
                        c.ty == IrType::Int && matches!(c.op, Op::Const(v) if (v as i32) != 0)
                    })
                {
                    return;
                }
                // A fused `ExactClassIs` (`prepare_fusion_tables`' guard half):
                // `(class_id, receiver)`, tested at the guard in the flags.
                let fused_class = self.fused_guard_exact_class_info(cond_id);
                let cond_homeless = fused.is_none()
                    && fused_class.is_none()
                    && self.home_dropped.get(cond_id as usize).copied().unwrap_or(false);
                let cond_slot = if fused.is_some() || fused_class.is_some() || cond_homeless {
                    0
                } else {
                    self.slot_of(cond_id)
                };
                // nio21 wave 5 (lane L2): the zero-divisor guard of a division
                // armed with implicit exception edges raises its
                // `ArithmeticException` into this method's own handler
                // (`emit_implicit_throw_pad`) instead of deopting. No deopt
                // point is built: this guard has no deopt exit.
                if let Some(plan) = self
                    .implicit_throw_plans
                    .get(&id)
                    .filter(|_| fused_class.is_none())
                    .cloned()
                {
                    if self.implicit_pad_frame_ok(bci) {
                        let jcc_fail = match fused {
                            Some((cc, a, b)) => {
                                self.emit_fused_int_compare(a, b);
                                cc.x64_cc() ^ 1
                            }
                            None => {
                                if cond_homeless {
                                    self.gp_load_value(RAX, cond_id);
                                } else {
                                    self.load_to_rax(cond_slot);
                                }
                                self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
                                0x84 // JZ: the guard fails when cond == 0
                            }
                        };
                        self.emit_implicit_throw_branch(
                            jcc_fail,
                            ImplicitKind::Arithmetic,
                            bci,
                            id,
                            &plan,
                        );
                        return;
                    }
                }

                // Build + box the deopt point for this guard (stable address,
                // baked below). The point's frame state comes from the safepoint
                // snapshot recorded for `bci` during IR building.
                //
                // Through `resume_bci`, like every other deopt this file
                // emits. `IrBuilder::splice_guard_seen` refuses a graph that
                // built a guard inside a splice, so today the mapping is a
                // no-op here — but this arm was the ONLY one resolving from a
                // raw bci, and a raw bci inside a spliced body is the exact
                // shape that produced
                // `ir-inline-turns-an-index-out-of-bounds-into-an-internalerror-FIXED-20260828.md`.
                // A fence and an asymmetry is one fence away from the bug;
                // agreeing with the other emitters costs nothing.
                // Tier proposal 27 phase 3, item 3 (round 11 wave 13, lane
                // irexc): asked of the RAW bci, before the splice mapping.
                // Round 16 (lane matrix, MX16-1): a range predicate resumes
                // at its loop header with the loop-entry state, and the OSR
                // entry stubs re-check it, so it is a resumable exit too.
                let vthread_boundary = matches!(
                    self.graph.nodes[cond_id as usize].op,
                    Op::VthreadBoundaryTest { .. }
                );
                let exact_guard = vthread_boundary || self.op_guard_is_exact(id, bci)
                    || (crate::ir_optimize::osr_remat_array_load_enabled()
                        && self.op_guard_is_range_predicate(id, bci));
                // Round 12 wave 3 (lane replay): an IR SITE TRAP (the builder
                // listed its pc in `Graph::site_trap_pcs` and its condition is
                // the constant 0) fires on every arrival, so the attempt it
                // stops never passed this bci: `first_arrival`, which the VM's
                // replay rule reads. A guard that only FOLDED to 0 is not
                // listed; an unrolled copy of its bytecode may have passed.
                let site_trap = fused.is_none()
                    && self.graph.site_trap_pcs.contains(&bci)
                    && node
                        .inputs
                        .get(1)
                        .and_then(|&c| self.graph.nodes.get(c as usize))
                        .is_some_and(|c| matches!(c.op, Op::Const(0)));
                let raw_bci = bci;
                let bci = self.resume_bci(bci);
                let first_arrival = site_trap && bci == raw_bci;
                // Round 13 wave 3 (lane framestate): inside a spliced body, the
                // callee's own frame over its parked callers when a chain
                // snapshot describes the guard's pc (`emit_deopt_unless` does
                // the same). A site trap is never inside a splice, so
                // `first_arrival` is `false` whenever this answers.
                let chain = self.splice_chain_for_site(raw_bci);
                let is_chain_point = chain.is_some();
                if !is_chain_point {
                    self.refuse_flat_at_chain_only_pc(raw_bci);
                }
                let (point_bci, frame_state) = match chain {
                    Some(chain) => (chain.bci, chain),
                    None => (bci as u32, self.resolve_frame_state_for_site(bci)),
                };
                // JIT r20 wave 19 (LOOPOPT20-6): a guard that boxed the flat
                // frame at `bci` is a by-bci transfer there (the pure-body
                // unroll's net; `transfer_bcis` never listed this arm).
                if is_chain_point || !self.cur_node_frame_is_at(bci) {
                    self.note_flat_transfer(bci, "op_guard", false);
                }
                self.latch_if_site_frame_undefined(is_chain_point, raw_bci, bci);
                // Round nio20 wave 2, lane L3 (proposal L1-2): a precise splice
                // exit for a class that was not loaded at compile time
                // (`Graph::class_loading_trap_pcs`, keyed by the RAW combined
                // pc) asks for a recompile on its first trap; the class is
                // loaded by then. Only a chain point: the exit is always inside
                // a splice, and a chainless point at that pc is someone else's.
                // The builder plants nothing else at such a pc (the exit IS
                // the translation of its `invokestatic` / `checkcast` /
                // `instanceof`), so the pc names the guard.
                let reason = if vthread_boundary {
                    if is_chain_point
                        || bci != raw_bci
                        || !self.cur_node_frame_is_at(bci)
                        || !crate::deopt::frame_state_is_resumable(&frame_state)
                    {
                        self.latch_bailout(Bailout::new(BailoutReason::Internal(
                            "virtual-thread boundary cannot resume at its bytecode",
                        )));
                        return;
                    }
                    self.vthread_boundaries_emitted.push(point_bci);
                    DeoptReason::TransferToInterpreter
                } else if is_chain_point
                    && self.graph.class_loading_trap_pcs.contains(&raw_bci)
                {
                    DeoptReason::ClassLoading
                } else if !is_chain_point
                    && bci == raw_bci
                    && self.graph.unstable_if_trap_pcs.contains(&raw_bci)
                {
                    // JIT r20 wave 11, lane pProf (W17L4-5): a never-taken
                    // prune guard (`Graph::unstable_if_trap_pcs`; the builder
                    // lists it only under its switch).
                    DeoptReason::UnstableIf
                } else {
                    DeoptReason::UncommonTrap
                };
                // JIT round 21 wave 1 (lane loops, LOOPS21-2,
                // `CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION`, opt-in): a
                // loop-entry predicate (`Graph::predicate_guard_kinds`, filled
                // only under the switch) carries its planter's own id, so its
                // traps withdraw that planter's predicate alone
                // (`crate::deopt::predicate_withdrawn`). Only a flat point at
                // its own bci with the plain reason; anything else keeps the
                // derived id below.
                let speculation_id = match self
                    .graph
                    .predicate_guard_kinds
                    .iter()
                    .find(|&&(g, _)| g == id)
                    .and_then(|&(_, k)| crate::deopt::PredicateKind::from_u8(k))
                {
                    Some(kind)
                        if !is_chain_point
                            && bci == raw_bci
                            && reason == DeoptReason::UncommonTrap =>
                    {
                        crate::deopt::predicate_speculation_id(bci as u32, kind)
                    }
                    _ => crate::deopt::speculation_id(bci as u32, reason),
                };
                // JIT round 23 wave 4 (lane L1): a loop-entry predicate of a
                // loop spliced from a callee (`Graph::chain_predicate_guards`)
                // whose chain this point publishes carries the chain id of
                // its innermost scope's header, whatever the own-speculation
                // switch says: it is how the VM's chain sinks tell it from
                // the splice's other guards (`deopt_resume::chain_predicate_charge_id`).
                let speculation_id = match self
                    .graph
                    .chain_predicate_guards
                    .iter()
                    .find(|&&(g, _)| g == id)
                    .and_then(|&(_, k)| crate::deopt::PredicateKind::from_u8(k))
                {
                    Some(kind)
                        if is_chain_point
                            && reason == DeoptReason::UncommonTrap
                            && !frame_state.method_key.is_empty() =>
                    {
                        crate::deopt::chain_predicate_speculation_id(
                            &frame_state.method_key,
                            point_bci,
                            kind,
                        )
                    }
                    _ => speculation_id,
                };
                let point = Box::new(DeoptimizationPoint {
                    native_offset: self.buf.pos() as u32,
                    bci: point_bci,
                    reason,
                    action: DeoptAction::Reinterpret,
                    // Derived, not `SPECULATION_ID_ANY`: this producer holds
                    // both halves of the key — the resume bci published on the
                    // line above and the reason beside it — so it can say WHICH
                    // speculation at that bytecode index this point describes.
                    // The derivation lives in `crate::deopt::speculation_id`
                    // because the CONSUMER derives the same value
                    // (`DespecRegistry::contains_speculation`, and the emitter
                    // gates that ask it); two spellings would be two ids.
                    //
                    // `(bci, reason)` and never a counter: a loop rewrite emits
                    // the body more than once, and
                    // `x64/loop_rewrite::deopt_point_difference` treats a
                    // `speculation_id` mismatch between two images of one
                    // bytecode as `PointDifference::Fatal`. That function's
                    // caller groups by `(bci, reason)`, so two points reaching
                    // the comparison agree on this field by construction.
                    // (LOOPS21-2: or the predicate planter's id, above; a
                    // pre-header predicate is emitted once.)
                    speculation_id,
                    frame_state,
                    // The guard fires BEFORE the bytecode it protects, so the
                    // interpreter re-runs that bytecode. `for_reason` is the
                    // convention this file already documents, now written down
                    // in the metadata instead of inferred by each consumer.
                    semantics: ResumeSemantics::for_reason(reason)
                        .with_first_arrival(first_arrival),
                });
                let point_ptr = point.as_ref() as *const DeoptimizationPoint as u64;
                self.deopt_boxes.push(point);
                // JIT round 19 wave 3 (lane eager3, EAGER2-4): the builder's
                // cause for a trap or class-init guard at this RAW pc, beside
                // the point (not inside a splice: resume == raw there).
                if !is_chain_point && bci == raw_bci && !self.graph.trap_causes.is_empty() {
                    if let Some(cause) = self.graph.trap_cause_at(raw_bci) {
                        if crate::ir::trap_cause_on_point_enabled() {
                            self.trap_cause_points.push((point_ptr, cause.code()));
                        }
                    }
                }
                // Counted beside the box it classifies, as `emit_deopt_unless`
                // does; `osr_exit::guard_exit_fact` compares the two.
                if exact_guard {
                    self.classified_guard_exits += 1;
                }

                // JIT rx27 wave 6 (lane regalloc): `(x & mask) == 0` as
                // `TEST x, mask`, and the `And`'s home rebuilt in the pad when
                // this guard's frame names it (`plan_and_zero_guard_tests`).
                let pads_before = self.deopt_pads.len();
                let and_test = self.test_guard_cond.get(cond_id as usize).copied().flatten();
                if let (Some((cc, _, _)), Some((x, imm, wide))) = (fused, and_test) {
                    let at = self.emit_and_zero_test(x, imm, wide);
                    let and_pads_before = self.deopt_pads.len();
                    self.emit_deopt_jcc_to_pad(cc.x64_cc() ^ 1, point_ptr);
                    let remat = |i: NodeId| self.test_and_remat.get(i as usize).copied();
                    let and_node = self
                        .graph
                        .nodes
                        .get(cond_id as usize)
                        .and_then(|c| c.inputs.iter().copied().find(|&i| remat(i) == Some(true)));
                    if let Some(a) = and_node {
                        match (self.slot_of_checked(a), self.deopt_pads.get(and_pads_before)) {
                            (Ok(slot), Some(&(patch, _))) => {
                                self.deopt_pad_remats.insert(
                                    patch,
                                    PadRemat {
                                        x_loc: at,
                                        imm,
                                        wide,
                                        slot,
                                        is_add: false,
                                    },
                                );
                            }
                            _ => self.latch_bailout(Bailout::with_context(
                                BailoutReason::UnallocatedValue { node: a },
                                format!("n{a}: its guard's pad cannot rebuild its home"),
                            )),
                        }
                    }
                    let never_passes = self.guard_is_failure_only(cond_id, bci, raw_bci);
                    self.emit_forced_guard_deopt(point_ptr, is_chain_point, never_passes, reason);
                    return;
                }
                if let Some((class_id, recv)) = fused_class {
                    // ZF=1 iff the class matches: the pad is taken on JNE.
                    self.gp_load_value(RAX, recv);
                    self.emit_exact_class_test_flags_for(recv, class_id);
                    self.emit_deopt_jcc_to_pad(0x85, point_ptr);
                    self.note_fused_exact_class_guard_proven(recv, class_id);
                    let never_passes = self.guard_is_failure_only(cond_id, bci, raw_bci);
                    self.emit_forced_guard_deopt(point_ptr, is_chain_point, never_passes, reason);
                } else if let Some((cc, a, b)) = fused {
                    // The guard passes when `a <cc> b`; `^ 1` inverts the x86
                    // condition, so the pad is taken exactly when the boolean
                    // the unfused path tests would have been 0.
                    self.emit_fused_int_compare(a, b);
                    self.emit_deopt_jcc_to_pad(cc.x64_cc() ^ 1, point_ptr);
                    // j27 wave 4 (lane DEOPTROWS, CH16-5): flat points too
                    // under `CRATONVM_DEOPT_EAGER_GUARDS`.
                    let never_passes = self.guard_is_failure_only(cond_id, bci, raw_bci);
                    self.emit_forced_guard_deopt(point_ptr, is_chain_point, never_passes, reason);
                } else {
                    // cond → RAX; TEST EAX, EAX
                    if cond_homeless {
                        self.gp_load_value(RAX, cond_id);
                    } else {
                        self.load_to_rax(cond_slot);
                    }
                    self.buf.emit(&[0x85, 0xC0]);
                    // JZ → out-of-line deopt pad (deopt when cond == 0); the
                    // passing guard falls straight through. See
                    // `emit_deopt_jcc_to_pad`.
                    self.emit_deopt_jcc_to_pad(0x84, point_ptr);
                    // Round 13 wave 11 (lane chain5, CH-1): a no-op unless
                    // `CRATONVM_DEOPT_EAGER_CHAINS` is on (j27 wave 4, CH16-5:
                    // flat points too under `CRATONVM_DEOPT_EAGER_GUARDS`, except
                    // a site trap, which never passes).
                    let never_passes = self.guard_is_failure_only(cond_id, bci, raw_bci);
                    self.emit_forced_guard_deopt(point_ptr, is_chain_point, never_passes, reason);
                }
                self.attach_add_pad_remats(id, pads_before);
            }
            // getfield read — `Op::Load`. The builder emits
            // `Op::Load(MemKind::Int)` for the int-category fields,
            // `Op::Load(MemKind::Ref)` for reference fields and (COV-03)
            // `Long`/`Float`/`Double` for the wide ones; all read through the
            // checked `jit_getfield` helper, which returns the int payload, the
            // raw pointer, the long payload or the FP bit pattern according to
            // the receiver's registered layout. The inline fallback below is
            // int-only and layout-naive, and `lower_inner` refuses any graph
            // that would need it for a reference or wide load. inputs = [ctrl,
            // mem, base, offset] where `offset` is a `Const(field_index)`.
            Op::Load(_) => {
                let slot = self.alloc_slot(id);
                let base = node.inputs[2];
                let offset_node = node.inputs[3];
                // `lower_inner` refuses the graph unless every field access's
                // offset edge is a `Const`, so the `None` arm is unreachable.
                // It deopts rather than falling back to slot `0`, which is what
                // it used to do: slot 0 is a DIFFERENT field of the same
                // receiver, read or written with no trace of the substitution.
                // A deopt is the one answer that is always correct here — the
                // interpreter re-executes the access against the real layout —
                // and it costs nothing on a path nothing reaches.
                let Op::Const(field_index) = self.graph.nodes[offset_node as usize].op else {
                    self.emit_unconditional_deopt(self.deopt_pc_of(node));
                    return;
                };
                // nio21 wave 6 (lane L2, proposal L2W5-1): a `getfield` armed
                // with implicit exception edges tests its receiver FIRST and
                // sends a null one to the inline pad that asks this method's
                // own handlers (`emit_implicit_throw_pad`, an NPE whose JEP 358
                // message is rebuilt from the bytecode at the trap site)
                // instead of letting an arm below deopt or leave through the
                // helper's sentinel. Every arm below loads the receiver itself,
                // so their own null handling is then only ever reached by a
                // non-null receiver. Not armed: nothing is emitted here.
                if let Some(pc) = node.bytecode_pc {
                    if let Some(plan) = self
                        .implicit_throw_plans
                        .get(&id)
                        .cloned()
                        .filter(|_| self.implicit_pad_frame_ok(pc))
                    {
                        let receiver = self.bind_field_receiver(node.ty, base);
                        self.emit_field_receiver_test(receiver);
                        self.emit_implicit_throw_branch(0x84, ImplicitKind::Npe, pc, id, &plan);
                    }
                }
                // Guarded inline read of a compact field, with the checked
                // helper as the slow path — the same trade the single-pass
                // backend makes. Returns false when this site is not eligible
                // (no resolved compact slot, no published region bounds, gate
                // off, or a width this arm does not emit), leaving the helper
                // path below untouched.
                if self.emit_inline_compact_getfield(
                    node.bytecode_pc,
                    node.ty,
                    base,
                    field_index,
                    slot,
                    node.is_volatile_access(),
                ) {
                    // Round 11 (irlower): an FP result reaches its XMM register
                    // the way the helper arm below publishes it. The inline arm
                    // stored only the home word, so an XMM-resident `float` /
                    // `double` field value was re-read from memory at every use.
                    match node.ty {
                        IrType::Float => self.publish_fp_from_slot(id, slot, false),
                        IrType::Double => self.publish_fp_from_slot(id, slot, true),
                        _ => {}
                    }
                    return;
                }
                if self.getfield != 0 {
                    crate::metrics::note_getfield_arm(5);
                    // See the inline-compact arm: `0` when this node carries no
                    // bytecode pc, which is also the "not described" key.
                    let npe_site = node
                        .bytecode_pc
                        .map_or(0, |pc| self.record_npe_trap_site(pc));
                    // JIT round 25 item 2, wave 4: as the checked helper's
                    // other call site (`emit_getfield_checked_helper_call`).
                    let lazy = self.lazy_ref_hook_enter();
                    self.load_context(CALL_ARG_REGS[0]);
                    self.gp_load_value(CALL_ARG_REGS[1], base);
                    // Reference loads must carry `GETFIELD_EXPECT_REFERENCE` at
                    // EVERY arm, not just the one that crashed — this is the
                    // inline-compact fallback arm. No receiver proof is claimed
                    // here.
                    self.emit_mov_reg_imm64(
                        CALL_ARG_REGS[2],
                        // The trap-site key — see the sibling arm above.
                        cratonvm_jit_api::getfield_index_arg(
                            field_index as u32,
                            node.ty == IrType::Ref,
                            false,
                            npe_site,
                        ) | node
                            .bytecode_pc
                            .map_or(0, |pc| self.getfield_no_keepalive_bit(pc)),
                    );
                    self.emit_mov_reg_imm64(RAX, self.getfield as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                                                  // The checked `jit_getfield` helper returns the `i64::MIN`
                                                  // deopt/NPE sentinel (with the pending-NPE flag set) on a bad
                                                  // receiver instead of a legitimate field value. For an
                                                  // int-category or reference field `i64::MIN` can never be a
                                                  // genuine result (no plausible heap pointer equals it), so a
                                                  // plain compare-and-bail is unambiguous. For a `J`/`D` field
                                                  // it CAN be — `Long.MIN_VALUE`, and `-0.0`, whose bits are
                                                  // exactly `i64::MIN` — so COV-03 reuses the same out-of-band
                                                  // `jit_dispatch_threw` peek `Op::Call` already uses for a
                                                  // `J`/`D` return. Without either, a bad receiver silently
                                                  // corrupts execution instead of throwing (crash → hang
                                                  // conversion).
                    self.emit_helper_sentinel_check(node.ty);
                    self.lazy_ref_hook_exit(lazy);
                    self.store_rax(slot);
                    // A `Float`/`Double` result arrives in RAX as raw bits and
                    // its home word now holds them; republish into the value's
                    // XMM register, exactly as `Op::ConstF` / FP `Op::Param` do.
                    match node.ty {
                        IrType::Float => self.publish_fp_from_slot(id, slot, false),
                        IrType::Double => self.publish_fp_from_slot(id, slot, true),
                        _ => {}
                    }
                } else {
                    // Byte displacement of the field's 32-bit Int payload within
                    // the object: HEADER_SIZE + field_index*SLOT_SIZE +
                    // FIELD_CELL_PAYLOAD32_OFFSET (the same arithmetic the
                    // single-pass inline getfield uses).
                    let disp = HEADER_SIZE as i32
                        + (field_index as i32) * SLOT_SIZE as i32
                        + FIELD_CELL_PAYLOAD32_OFFSET as i32;
                    // Receiver pointer → RAX (64-bit; a Param slot holds the full
                    // pointer the prologue stored from the argument register).
                    self.gp_load_value(RAX, base);
                    // TEST RAX,RAX ; a null receiver DEOPTS, exactly as the
                    // `Op::Store` int arm does, and the interpreter re-executes
                    // the getfield and throws NullPointerException.
                    //
                    // Round 9 wave 2 (iropt X3): this used to be `JE → XOR
                    // EAX,EAX`, answering `0` for a null `getfield` and carrying
                    // on -- a silent NPE-to-zero. Reachable only with no
                    // `getfield` helper and compact fields off (partial helper
                    // tables, tests), but wrong wherever it is reachable.
                    self.buf.emit(&[0x48, 0x85, 0xC0]);
                    self.emit_deopt_if_zero(self.deopt_pc_of(node), DeoptReason::NullCheck);
                    // MOVSXD RAX, [RAX + disp32]  (sign-extend the Int payload).
                    self.buf.emit(&[0x48, 0x63, 0x80]);
                    self.buf.emit(&disp.to_le_bytes());
                    // Spill the result.
                    self.store_rax(slot);
                }
            }
            // putfield write — `Op::Store`. `MemKind` selects the lowering:
            // `Int` is the inline heap write below (or `jit_putfield_int` under
            // compact layout), `Ref` is ALWAYS `jit_putfield_object` (COV-03 —
            // the barrier), and `Long`/`Float`/`Double` are the matching
            // `jit_putfield_*` helper. The inline int write: a null receiver
            // DEOPTS (the interpreter then re-executes this putfield and throws
            // NullPointerException), else write a `Value::Int(value)` cell
            // (discriminant 0 + the 32-bit payload, high qword cleared so no
            // stale ref/garbage survives — mirroring the scalar-replace store
            // and the real helper). inputs = [ctrl, mem, base, offset, value];
            // produces no value (a pure memory-ordering token), so no slot is
            // allocated.
            Op::Store(kind) => {
                // JIT round 19 wave 5 (lane osr5, OSR2-3): an in-loop store of
                // a promoted cell kept only for its position and its value's
                // liveness (`Graph::promoted_sunk_stores`): it emits nothing;
                // the exit and every deopt pad of the loop write the cell.
                if self.is_silent_promoted_store(id) {
                    return;
                }
                let base = node.inputs[2];
                let offset_node = node.inputs[3];
                let value = node.inputs[4];
                // Unreachable for the same reason the `Op::Load` arm's is, and
                // deopts for the same reason — see there.
                let Op::Const(field_index) = self.graph.nodes[offset_node as usize].op else {
                    self.emit_unconditional_deopt(self.deopt_pc_of(node));
                    return;
                };
                let tag_off = HEADER_SIZE as i32 + (field_index as i32) * SLOT_SIZE as i32;
                let pay_off = tag_off + FIELD_CELL_PAYLOAD32_OFFSET as i32;
                let high_off = tag_off + 8; // the 8-byte payload region (Long/ref)
                let bci = self.deopt_pc_of(node);
                // JIT r23 wave 11b (lane L5): this store's own compact row, for
                // the inline emitters below at a synthetic pc
                // (`synthetic_store_row`); every return of this arm leaves it
                // set only until the next `Op::Store` overwrites it, and no
                // other node reads it.
                self.synthetic_store_row = self.graph.field_rows.get(&id).copied();
                // COV-03 — a REFERENCE store. There is no inline route and no
                // layout-conditional choice to make: `jit_putfield_object` is
                // the single-pass backend's own full-barrier fallback, it is
                // compact-aware in its own right, and it is what carries the
                // SATB pre-barrier on the OLD reference plus the collector's
                // post-write barrier. A missing barrier is invisible until a
                // concurrent or generational collection reclaims a still-live
                // object, so this tier does not get a barrier-free fast path
                // until it can prove the same premises `x64::objects` proves
                // (mapped, genuinely compact, YOUNG receiver whose old field is
                // null, with live region bounds published).
                //
                // The null check stays INLINE and deopts, for the same reason
                // the int path's does: the helper returns silently on an
                // implausible receiver, so calling it unguarded would convert a
                // NullPointerException into a dropped store.
                //
                // Every route's check goes through
                // `emit_store_receiver_null_check`, which omits it for a
                // receiver that cannot be null (a fresh allocation) -- and then
                // this store emits no deopt at all, which `store_cannot_deopt`
                // predicts for `ir_optimize` (round 9 wave 4, L3).
                if matches!(kind, MemKind::Ref) {
                    self.gp_load_value(RAX, base);
                    self.emit_store_receiver_null_check_for(base, bci, id);
                    // GATED reference store — tried first, and complete when it
                    // takes: the barrier-free store, both gate sequences, the
                    // helper fallback and the out-of-bounds drop all converge
                    // at its end. `false` means "not admitted", and the
                    // unconditional helper call below then runs exactly as it
                    // did before. See `emit_gated_ir_ref_putfield`.
                    let fresh = self.zgc_fresh_stores.contains(&id);
                    if self.emit_gated_ir_ref_putfield(
                        node.bytecode_pc,
                        base,
                        value,
                        field_index,
                        fresh,
                    ) {
                        return;
                    }
                    // jit_putfield_object(vm_ptr, obj_ptr, field_index, val).
                    // Unlike every other putfield helper it takes the context
                    // pointer; `scan_frame_needs` reserves the slot for it.
                    self.load_context(CALL_ARG_REGS[0]);
                    self.gp_load_value(CALL_ARG_REGS[1], base);
                    self.emit_mov_reg_imm64(CALL_ARG_REGS[2], field_index as i64 as u64);
                    self.gp_load_value(CALL_ARG_REGS[3], value);
                    self.emit_mov_reg_imm64(RAX, self.putfield_object as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    return;
                }
                // COV-03 — a WIDE store (`J`/`F`/`D`). Same shape as the compact
                // int store: inline null check + deopt, then the width's own
                // `(obj_ptr, field_index, bits)` helper, which resolves the
                // packed offset and writes a correctly-tagged cell. An FP value
                // rides a GPR as raw bits, which is how its frame slot already
                // holds it, so the ordinary integer slot load is the marshal.
                let wide_helper = match kind {
                    MemKind::Long => Some(self.putfield_long),
                    MemKind::Float => Some(self.putfield_float),
                    MemKind::Double => Some(self.putfield_double),
                    _ => None,
                };
                if let Some(helper) = wide_helper {
                    if self.try_emit_allocated_prim_putfield(
                        id,
                        bci,
                        node.bytecode_pc,
                        *kind,
                        base,
                        value,
                    ) {
                        return;
                    }
                    self.gp_load_value(RAX, base);
                    self.emit_store_receiver_null_check_for(base, bci, id);
                    // Round 11 wave 2 (lane lower): the guarded inline store
                    // first; its declined paths land on the helper call below.
                    let inline_done = self.emit_inline_prim_putfield(
                        node.bytecode_pc,
                        *kind,
                        base,
                        value,
                        field_index,
                    );
                    if matches!(inline_done.as_deref(), Some([])) {
                        return;
                    }
                    self.gp_load_value(CALL_ARG_REGS[0], base);
                    self.emit_mov_reg_imm64(CALL_ARG_REGS[1], field_index as i64 as u64);
                    self.gp_load_value(CALL_ARG_REGS[2], value);
                    self.emit_mov_reg_imm64(RAX, helper as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    for p in inline_done.into_iter().flatten() {
                        self.patch_rel32_to_here(p);
                    }
                    return;
                }
                // Compact layout packs field offsets, so the uniform
                // `field_index * SLOT_SIZE` displacement below is wrong for a
                // compact object. Route the write through `jit_putfield_int`,
                // which resolves the packed offset from the receiver's
                // registered layout — the same helper the baseline tier uses.
                //
                // The null check stays INLINE and still deopts. The helper
                // returns silently on an implausible receiver, so calling it
                // unguarded would convert a NullPointerException into a
                // dropped store — the exact silent-data-loss defect the inline
                // path was fixed for in cd451faccc.
                if cratonvm_types::compact_ref_fields_enabled() {
                    if self.try_emit_allocated_prim_putfield(
                        id,
                        bci,
                        node.bytecode_pc,
                        *kind,
                        base,
                        value,
                    ) {
                        return;
                    }
                    self.gp_load_value(RAX, base);
                    self.emit_store_receiver_null_check_for(base, bci, id);
                    // Round 11 wave 2 (lane lower): the guarded inline store
                    // first; its declined paths land on the helper call below.
                    let inline_done = self.emit_inline_prim_putfield(
                        node.bytecode_pc,
                        *kind,
                        base,
                        value,
                        field_index,
                    );
                    if matches!(inline_done.as_deref(), Some([])) {
                        return;
                    }
                    // jit_putfield_int(obj_ptr, field_index, val) — no context.
                    // Receiver first: `load_reg_from_frame` into arg0 would be
                    // clobbered by nothing here, but keep the order arg0..arg2
                    // so a future stack-arg spill sees a conventional sequence.
                    self.gp_load_value(CALL_ARG_REGS[0], base);
                    self.emit_mov_reg_imm64(CALL_ARG_REGS[1], field_index as i64 as u64);
                    self.gp_load_value(CALL_ARG_REGS[2], value);
                    self.emit_mov_reg_imm64(RAX, self.putfield_int as u64);
                    self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                    for p in inline_done.into_iter().flatten() {
                        self.patch_rel32_to_here(p);
                    }
                    return;
                }
                let receiver = if !self.carry_names(base) {
                    if let Some(reg) = self
                        .resident_gpr(base)
                        .filter(|r| IR_LOWER_LS_GPRS.contains(r) || IR_LS_CALLER_SAVED_GPRS.contains(r))
                    {
                        self.gp_reg_reads += 1;
                        reg
                    } else {
                        self.gp_load_value(RAX, base);
                        RAX
                    }
                } else {
                    self.gp_load_value(RAX, base);
                    RAX
                };
                let val_reg = if !self.carry_names(value) {
                    if let Some(reg) = self
                        .resident_gpr(value)
                        .filter(|r| (IR_LOWER_LS_GPRS.contains(r) || IR_LS_CALLER_SAVED_GPRS.contains(r)) && *r != receiver)
                    {
                        self.gp_reg_reads += 1;
                        reg
                    } else {
                        self.gp_load_value(RCX, value);
                        RCX
                    }
                } else {
                    self.gp_load_value(RCX, value);
                    RCX
                };
                // A null receiver is a NullPointerException, not a no-op. This
                // arm used to `JE` over the store, silently dropping it and
                // continuing — the same silent-data-loss defect fixed in the
                // single-pass backend in cd451faccc, which this tier still had.
                // Deopt instead, exactly as the `ArrayLoad`/`ArrayStore` guards
                // below already do for a null array: control leaves for the
                // shared deopt stub, and the interpreter re-executes this
                // putfield and throws. No new machinery, and the store body is
                // no longer required to be a fixed 27 bytes.
                self.emit_store_receiver_null_check_at(base, bci, id, receiver);
                // MOV dword [receiver + tag_off], 0   (Value::Int discriminant)
                if receiver == RAX {
                    self.buf.emit(&[0xC7, 0x80]);
                    self.buf.emit(&tag_off.to_le_bytes());
                    self.buf.emit(&0u32.to_le_bytes());
                } else {
                    use crate::x64::isel::{Mem, Op as MachineOp, Operand, Req, Ty};
                    self.emit_field_instruction(Req::new(
                        MachineOp::Mov,
                        Ty::I32,
                        Operand::Mem(Mem::base_disp32(receiver, i64::from(tag_off))),
                        Operand::Imm(0),
                    ));
                }
                // MOV dword [receiver + pay_off], val_reg (Int payload)
                if receiver == RAX && val_reg == RCX {
                    self.buf.emit(&[0x89, 0x88]);
                    self.buf.emit(&pay_off.to_le_bytes());
                } else {
                    use crate::x64::isel::{Mem, Op as MachineOp, Operand, Req, Ty};
                    self.emit_field_instruction(Req::new(
                        MachineOp::Mov,
                        Ty::I32,
                        Operand::Mem(Mem::base_disp32(receiver, i64::from(pay_off))),
                        Operand::Gpr(val_reg),
                    ));
                }
                // MOV qword [receiver + high_off], 0  (clear high qword)
                if receiver == RAX {
                    self.buf.emit(&[0x48, 0xC7, 0x80]);
                    self.buf.emit(&high_off.to_le_bytes());
                    self.buf.emit(&0u32.to_le_bytes());
                } else {
                    let rex_b = if receiver >= 8 { 1 } else { 0 };
                    if (receiver & 7) == 4 {
                        self.buf.emit(&[0x48 | rex_b, 0xC7, 0x84, 0x24]);
                    } else {
                        self.buf.emit(&[0x48 | rex_b, 0xC7, 0x80 | (receiver & 7)]);
                    }
                    self.buf.emit(&high_off.to_le_bytes());
                    self.buf.emit(&0u32.to_le_bytes());
                }
                // (the store body no longer needs a fixed size: the null path
                // exits to the deopt stub rather than jumping over these bytes)
            }
            // FP array element load (Slice B) — `faload`/`daload`. inputs =
            // [ctrl, mem, array, index]. Load the array pointer to RAX and the
            // index to RCX (the layout the SIB access expects), emit the JVMS
            // null + bounds deopt guards, then `MOVSS`/`MOVSD` the element from
            // `[RAX + RCX*elem_size + HEADER_SIZE]` into XMM0 and spill it.
            //
            // The guard normalizes the index with `MOV ECX, ECX` before it
            // checks anything, so by the time the 32-bit unsigned bounds CMP
            // runs the high half of RCX is zero and the SIB's 64-bit scale
            // sees exactly the value that was checked. This comment used to
            // argue the same conclusion from "the index slot holds a
            // sign-extended int" instead. That premise was an unenforced
            // convention rather than an invariant — the REX.W `iand`/`ior`/
            // `ixor` arms produced index words that were neither the sign- nor
            // the zero-extension of their low half — and the argument silently
            // stopped holding. The normalization is what makes it true now.
            //
            // FP element accesses never alias the int field cells, but the
            // memory-token edge still serialises them with neighbours.
            Op::ArrayLoad(kind) => {
                let slot = self.alloc_slot(id);
                // JIT r20 wave 19 (LOOPOPT20-2): a full pure-body unroll's
                // pc-less copy of a masked load is proven by its guard token
                // (`MaskGuardProofs::proves`), so neither check below is
                // emitted and it owes no resume pc. Anything else keeps
                // `deopt_pc_of`'s rule (a pc-less node that can deopt refuses
                // the compile).
                let bci = match node.bytecode_pc {
                    None if self.mask_guard_proven(node) => self.cur_bci,
                    _ => self.deopt_pc_of(node),
                };
                // zt t19/zb2 (zr1br A2b read side): under an armed ZGC load
                // barrier a reference element is read by `jit_aaload`, which
                // reads it through `VmHeap::load_ref_slot_barriered` -- row 30
                // of the t18/zb1 read inventory. See
                // `lower_ref_array_load_through_barrier_helper`.
                // zt t21/zb4 (stage 2d): unless the colored inline decode
                // serves it -- then the inline arm below runs and
                // `emit_gpr_array_elem_load` appends the decode.
                if matches!(kind, MemKind::Ref) && crate::x64::zgc_ref_element_load_calls_helper() {
                    self.lower_ref_array_load_through_barrier_helper(id, node, slot, bci);
                    return;
                }
                // COV-02: everything that is not `float`/`double` lands the
                // element in a GPR and spills it exactly as the single-pass
                // backend does. The guards, the SIB base/index registers and
                // the header displacement are identical for every width — the
                // only thing that varies is one instruction.
                if !matches!(kind, MemKind::Float | MemKind::Double) {
                    let direct_result = if matches!(
                        kind,
                        MemKind::Byte | MemKind::Char | MemKind::Short | MemKind::Int | MemKind::Long
                    ) {
                        self.integer_result_gpr(node.ty, slot)
                    } else if matches!(kind, MemKind::Ref)
                        && self.ref_residency_on
                        && !narrow_oops_enabled()
                        && !crate::x64::zgc_read_barrier_blocks_inline_fields()
                    {
                        self.reference_result_gpr(slot)
                    } else {
                        None
                    };
                    let (base, index) = self.bind_checked_array_load_address(id, node, bci);
                    // Preserve both address operands for any later live use.
                    let direct_result = if matches!(kind, MemKind::Ref) {
                        direct_result.filter(|&dst| dst != base && dst != index)
                    } else {
                        direct_result
                    };
                    if matches!(kind, MemKind::Ref) {
                        if let Some(dst) = direct_result {
                            self.emit_plain_ref_array_elem_load_to(dst, base, index);
                        } else {
                            self.emit_ref_array_elem_load_at(base, index, false);
                        }
                    } else if matches!(kind, MemKind::Byte)
                        && self
                            .unsigned_byte_array_loads
                            .get(id as usize)
                            .copied()
                            .unwrap_or(false)
                    {
                        self.emit_str_char_load_to(
                            MemKind::Byte,
                            direct_result.unwrap_or(RAX),
                            base,
                            index,
                        );
                    } else if direct_result.is_some() || base != RAX || index != RCX {
                        self.emit_gpr_primitive_array_elem_load_to(
                            *kind, direct_result.unwrap_or(RAX), base, index,
                        );
                    } else {
                        self.emit_gpr_array_elem_load(*kind);
                    }
                    if let Some(dst) = direct_result {
                        self.publish_integer_result(dst, slot);
                        if matches!(kind, MemKind::Ref) { self.ref_def_home_stores += 1; }
                    } else {
                        self.store_rax(slot);
                    }
                    return;
                }
                let is_d = matches!(kind, MemKind::Double);
                let (base, index) = self.bind_checked_array_load_address(id, node, bci);
                self.emit_fp_array_elem_access(is_d, false, base, index);
                self.fp_store_value(id, slot, XMM0, is_d);
            }
            // JIT round 21, lane charat (STRINTR21-5): the String char read.
            // inputs = [ctrl, mem, value, index, coder(, guard)]. No check of
            // any kind: the builder's guards (or the dominating facts
            // `ir_check_elim` elided them for) prove `value` non-null and
            // `0 <= index < value.length >>> coder` before this node runs.
            //
            // Test the coder in EDX, then load a zero-extended byte or
            // word using the same base and the normalized RCX index. A
            // resident backing array can serve both arms in place.
            Op::StrCharDecode => {
                let slot = self.alloc_slot(id);
                let direct_result = self.integer_result_gpr(node.ty, slot);
                self.gp_load_value(RDX, node.inputs[4]); // coder
                let value = node.inputs[2];
                let base = if self.reg_alloc_on && !self.carry_names(value) {
                    self.resident_gpr(value)
                } else {
                    None
                };
                let base = match base {
                    Some(reg) => {
                        self.gp_reg_reads += 1;
                        reg
                    }
                    None => {
                        self.gp_load_value(RAX, value);
                        RAX
                    }
                };
                let dst = direct_result.unwrap_or(RAX);
                let index = self.load_array_index(node.inputs[3], true);
                if index == RCX {
                    self.normalize_rcx_index();
                }
                self.buf.emit(&[0x85, 0xD2]); // TEST EDX, EDX
                let utf16 = self.emit_jcc_rel32(0x85); // JNZ utf16
                self.emit_str_char_load_to(MemKind::Byte, dst, base, index);
                let done = self.emit_jmp_rel32();
                self.patch_rel32_to_here(utf16);
                self.emit_str_char_load_to(MemKind::Char, dst, base, index);
                self.patch_rel32_to_here(done);
                if let Some(dst) = direct_result {
                    self.publish_integer_result(dst, slot);
                } else {
                    self.store_rax(slot);
                }
            }
            // FP array element store (Slice B) — `fastore`/`dastore`. inputs =
            // [ctrl, mem, array, index, value]. Load the value into XMM0 first
            // (it must survive the guards; the bounds check clobbers only a GPR
            // scratch), then array→RAX, index→RCX, the null + bounds guards, and
            // `MOVSS`/`MOVSD` XMM0 into the element. Produces a memory token (no
            // result slot is read), but a slot is allocated for layout uniformity.
            Op::ByteArrayCopy => {
                let _slot = self.alloc_slot(id);
                let (src, src_pos, dst, dst_pos, len) = (
                    node.inputs[2],
                    node.inputs[3],
                    node.inputs[4],
                    node.inputs[5],
                    node.inputs[6],
                );
                self.note_rcx_written();
                self.gp_load_value(RAX, src);
                self.gp_load_value(R10, src_pos);
                self.buf.emit(&[0x4D, 0x63, 0xD2]); // MOVSXD R10, R10D
                self.buf.emit(&[0x4E, 0x8D, 0x54, 0x10, 0x10]); // LEA R10, [RAX + R10 + 16]
                self.gp_load_value(RDX, dst);
                self.gp_load_value(RCX, dst_pos);
                self.buf.emit(&[0x48, 0x63, 0xC9]); // MOVSXD RCX, ECX
                self.buf.emit(&[0x48, 0x8D, 0x4C, 0x0A, 0x10]); // LEA RCX, [RDX + RCX + 16]
                self.gp_load_value(R11, len);
                self.buf.emit(&[0x4D, 0x63, 0xDB]); // MOVSXD R11, R11D
                self.buf.emit(&BYTE_ARRAYCOPY_LOOPS);
            }
            Op::ArrayStore(kind) => {
                let _slot = self.alloc_slot(id);
                let bci = self.deopt_pc_of(node);
                // COV-02: the integral widths take the value in RDX, which
                // `emit_array_null_bounds_guards` does not touch (it uses
                // RAX/RCX and R10), so it can be loaded before the guards for
                // the same reason XMM0 is on the FP path.
                //
                // `MemKind::Ref` (`aastore`, which `IrBuilder::build` does
                // emit) takes its own route below: that store owes a
                // covariance check and both GC barriers.
                if !matches!(kind, MemKind::Float | MemKind::Double) {
                    if matches!(kind, MemKind::Ref) {
                        // `aastore`. The sequence owes a JVMS 6.5 covariance
                        // check, an SATB pre-barrier and a card mark, and
                        // `x64/objects.rs`'s `inline_card_mark_available` was
                        // hard-`false` precisely so no emitter takes the
                        // remembered-set contract into itself. (gcg g4/jm:
                        // `CRATONVM_JIT_INLINE_CARD_MARK` is default ON since
                        // the gcg triage and this tier reads it too, as a
                        // CHECK of an already-dirty card,
                        // `emit_ir_aastore_card_arm`.) So this tier stores
                        // inline only where the published gates or a dirty
                        // card prove no barrier has work
                        // (`emit_gated_ir_aastore`, round 11 wave 5; since
                        // gcg g6/jm the card-store form also marks a clean
                        // card inline), and otherwise does what follows.
                        //
                        // What it does instead is the escape hatch the
                        // single-pass backend already uses for the same opcode
                        // under the ZGC barrier gate: call `jit_aastore`, which
                        // owns all three. Before this, `Op::ArrayStore(Ref)`
                        // failed closed and the method was refused -- **35
                        // methods on the H2 JDBC workload**, the second-largest
                        // build refusal there. A helper call is worse than the
                        // single-pass inline sequence and far better than no
                        // optimizing body at all.
                        //
                        // Three obligations, each of which is a silent bug if
                        // skipped:
                        //
                        // * `jit_aastore` ALLOCATES (it builds an
                        //   `ArrayStoreException` on the covariance-refusal
                        //   path), so the call is a GC point and needs an oop
                        //   map published at it -- the same bracketing
                        //   `Op::New`'s stub and the safepoint poll use.
                        // * it takes the VM context as arg0, so
                        //   `scan_frame_needs` must set `needs_context` for
                        //   this node, exactly as it does for
                        //   `Op::Store(MemKind::Ref)`.
                        // * it returns `()`, so RAX is UNDEFINED on return and
                        //   NO deopt-sentinel compare may be emitted after it.
                        //   Its exceptions travel by the pending-signal channel
                        //   the interpreter drains on return. This is the same
                        //   note `x64/bytecode_walk.rs`'s arm carries, and for
                        //   the same reason.
                        if self.aastore == 0 || !ir_aastore_enabled() {
                            self.latch_bailout(Bailout::with_context(
                                BailoutReason::UnsupportedShape(
                                    "ir_lower: ArrayStore(Ref) needs the aastore helper",
                                ),
                                format!(
                                    "n{id} is an aastore; helper={} gate={}",
                                    self.aastore,
                                    ir_aastore_enabled(),
                                ),
                            ));
                            return;
                        }
                        // ORDER IS LOAD-BEARING, and getting it wrong is how
                        // this arm SIGSEGV'd on the H2 workload the first time:
                        // the map goes FIRST, the arguments SECOND.
                        //
                        // `emit_safepoint_map` reaches `emit_shadow_push`,
                        // which clobbers RAX/RCX/R10/R11 and CALLS
                        // `get_current_thread` -- and a call clobbers every
                        // volatile register, which on Win64 is exactly
                        // `CALL_ARG_REGS` (RCX, RDX, R8, R9). Marshalling
                        // first and mapping second therefore handed
                        // `jit_aastore` four destroyed arguments, the first of
                        // which it dereferences as a `SharedVm`.
                        //
                        // Mapping first is safe and is what the safepoint poll
                        // already does: this backend homes every live value in
                        // a frame slot, so the map is a set of frame offsets
                        // that does not depend on any register, and the
                        // residency file is callee-saved (RBX, R12-R15) so it
                        // survives the shadow push's call by the ABI.
                        // Round 9: the JVMS null and bounds checks, INLINE and
                        // BEFORE the map, exactly as every other `ArrayStore`
                        // width does. `jit_aastore` returns `()`: on a null
                        // array or a bad index it only sets a pending signal
                        // and returns, and this body used to carry on — every
                        // later side effect ran, and the exception surfaced at
                        // method return from the wrong place (a
                        // `catch (ArrayIndexOutOfBoundsException)` around a
                        // store loop never fired, so the loop never ended).
                        // The single-pass helper route keeps its inline checks
                        // for the same reason (`x64/op_array.rs`, "emitted
                        // twice on this path"). A deopt here re-executes the
                        // `aastore` in the interpreter, which throws with the
                        // method's own handler semantics. Before the map
                        // because the guards clobber RAX/RCX/R10 and the map's
                        // shadow push clobbers them again; the arguments below
                        // are loaded after both.
                        //
                        // Round 9 wave 2: the covariance refusal
                        // (ArrayStoreException) no longer travels by the
                        // pending-signal channel either. Before `jit_aastore`,
                        // a non-null value is checked by
                        // `jit_aastore_type_check`, which returns a DEFINED
                        // `0 | i64::MIN`; the sentinel leaves through the shared
                        // exception epilogue with this bci stamped, exactly as
                        // `checkcast`'s ClassCastException does, so the
                        // method's own `catch (ArrayStoreException)` fires at
                        // the store and nothing after it runs. The single-pass
                        // inline arm has made the same call all along
                        // (`x64/op_array.rs`). Without the inline-store gates
                        // one map serves both calls: the check's return copies
                        // the shadow words back into the homes
                        // (`emit_pending_shadow_copy_back`, which leaves them
                        // published), so `jit_aastore`'s arguments are re-read
                        // post-collection, and the one reload below retracts.
                        // With them, each call path maps and reloads for
                        // itself (wave 14). The refusal exit skips the reload the way
                        // every call-exception exit does: the epilogue restores
                        // the shadow `top`.
                        let skip_guard_loads = self.check_elision.null_elided(id)
                            && self.check_elision.bounds_elided(id)
                            && !self.implicit_throw_plans.contains_key(&id)
                            && !self.carry_names(node.inputs[2])
                            && !self.carry_names(node.inputs[3])
                            && !self
                                .tail_carry
                                .is_some_and(|(p, _, _)| p == node.inputs[2] || p == node.inputs[3]);
                        if !skip_guard_loads {
                            self.gp_load_value(RAX, node.inputs[2]); // array → RAX
                            self.gp_load_index_rcx(node.inputs[3]); // index → RCX, zero-extended
                        }
                        self.emit_array_null_bounds_guards_for(bci, id);
                        let covariance_known_safe = aastore_covariance_known_safe(
                            self.graph,
                            node.inputs[2],
                            node.inputs[4],
                        );
                        let needs_type_check =
                            self.aastore_type_check != 0 && !covariance_known_safe;
                        // Round 11 wave 5 (lane lower, same page, proposal
                        // W4-2): the gated inline element store, with
                        // `jit_aastore` as its barriered path. The store is
                        // the site's only work and cannot collect, so the map
                        // and its shadow push move onto the helper path with
                        // their reload. Round 11 wave 14 (lane hashmap): that
                        // now holds with a type check in front too. The check
                        // maps, copies back and reloads on its OWN call path
                        // (below), so its null and exact-component exits reach
                        // the store unmapped, as a checkless site does.
                        // Without the gates the map stays first and the one
                        // reload sits at the join. Either way every push has
                        // its reload on the same path.
                        // The inline store's entry contract is a discharged
                        // covariance obligation. With no check helper wired
                        // (an embedder's table; the VM always wires it) a
                        // value of unknown class has not been checked, so
                        // only `jit_aastore`, which checks for itself, may
                        // store it (round 11 wave 14).
                        let covariance_discharged =
                            self.aastore_type_check != 0 || covariance_known_safe;
                        let inline_gates = if covariance_discharged {
                            self.gated_ir_aastore_gates()
                        } else {
                            None
                        };
                        // r21 barriers (BARRIERS21-2, opt-in): under G1 (no
                        // published plan) the G1 inline store, with
                        // `jit_aastore` as its barriered path, as the
                        // generational gated store has.
                        let g1_inline = if covariance_discharged && inline_gates.is_none() {
                            self.ir_g1_aastore_plan()
                        } else {
                            None
                        };
                        let map_on_helper_path = inline_gates.is_some() || g1_inline.is_some();
                        // Not behind `ir_gc_point_maps_enabled`: that switch
                        // restores the historical map-less poll and `Op::New`
                        // only (its doc). An unmapped `aastore` under it would
                        // leave an EARLIER safepoint's id in the slot while
                        // both helpers below can collect (round 11 wave 4).
                        let mut map = if map_on_helper_path {
                            None
                        } else {
                            self.emit_safepoint_map_if_enabled()
                        };
                        if needs_type_check {
                            // A null value is legal for every reference array:
                            // `arr[i] = null` skips the call entirely.
                            self.gp_load_value(RAX, node.inputs[4]); // value → RAX
                            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                            let null_value = self.emit_jcc_rel32(0x84); // JZ → store
                                                                        // Round 11 wave 14 (lane hashmap,
                                                                        // `r11w13-icwide-ir-aastore-exact-component-patch`):
                                                                        // the single-pass arm's exact-component fast path
                                                                        // (`x64/op_array.rs`). A reference array's header
                                                                        // class id IS its component class
                                                                        // (`typecheck::array_descriptor_of`), and a PLAIN
                                                                        // object's is its own, so equal ids prove the value
                                                                        // an instance of the component and the store legal
                                                                        // -- `tab[i] = newNode(..)` into `Node[]`, and every
                                                                        // node `resize` moves into the new table. An ARRAY
                                                                        // value is screened out first (its header also
                                                                        // names a component: `String[]` into `String[]`
                                                                        // would compare `String` with `String`).
                                                                        //
                                                                        // Unlike the single-pass twin, class id 0 on the
                                                                        // ARRAY is not sent to the helper. It is answered first, and
                                                                        // inline: it is `java/lang/Object`'s id
                                                                        // (`heap_types.rs`), and `aastore_store_is_refused`
                                                                        // answers "legal" for every value stored into such
                                                                        // an array (`reference_array_component_is_object`
                                                                        // is true for `Object` and for an unnameable
                                                                        // component alike). So `Object[]` stores --
                                                                        // `ArrayList.elementData`, varargs, `toArray` --
                                                                        // make no call either. Past that test the array's
                                                                        // id is non-zero, so an equal value id is non-zero
                                                                        // too and needs no test of its own.
                                                                        //
                                                                        // This only turns a helper "yes" into an inline
                                                                        // one; every other value calls exactly as before,
                                                                        // so ArrayStoreException, its bci and its ordering
                                                                        // are the helper's. Class id is the `u32` at header
                                                                        // offset 0, the word the `checkcast` fast path
                                                                        // below compares. Scratch RDX and R11: everything
                                                                        // after the join reloads its operands.
                                                                        // `CRATONVM_JIT_CHECKCAST_INLINE=0` (that fast
                                                                        // path's lever) withdraws it.
                            let mut exact_hit: Vec<usize> = Vec::new();
                            if crate::x64::checkcast_inline_enabled() {
                                self.gp_load_value(R11, node.inputs[2]); // array → R11
                                self.buf.emit(&[0x41, 0x83, 0x3B, 0x00]); // CMP DWORD [R11], 0
                                exact_hit.push(self.emit_jcc_rel32(0x84)); // JE → store
                                self.buf.emit(&[
                                    0xF6,
                                    0x40,
                                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                                    cratonvm_types::KIND_TAG_BYTE_MASK,
                                ]); // TEST BYTE [RAX+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK
                                let not_plain = self.emit_jcc_rel32(0x85); // JNE → helper
                                self.buf.emit(&[0x8B, 0x10]); // MOV EDX, [RAX]
                                self.buf.emit(&[0x41, 0x3B, 0x13]); // CMP EDX, [R11]
                                exact_hit.push(self.emit_jcc_rel32(0x84)); // JE → store
                                // JIT r21 wave 1 (lane typecheck,
                                // `CRATONVM_JIT_IR_AASTORE_DISPLAY_ARM`, opt-in):
                                // a plain value of a SUBCLASS of the array's
                                // component class, or of a class implementing
                                // the component interface, proven from the two
                                // supers displays ([`aastore_display_arm_bytes`]),
                                // also stores without the check call: HotSpot's
                                // aastore store check (component klass, then the
                                // primary / secondary supers of the value's).
                                // Only a YES; every other outcome takes the
                                // helper below unchanged, so the
                                // ArrayStoreException is still the helper's.
                                // EDX = the value's class id, R11 = the array;
                                // the arm writes RCX, RDX, R10 and the flags,
                                // and everything past the join reloads.
                                if let Some(cell) = self.aastore_display_cell() {
                                    self.note_rcx_written();
                                    self.buf.emit(&[0x41, 0x8B, 0x0B]); // MOV ECX, [R11] (component id)
                                    let (bytes, edges, hits) = aastore_display_arm_bytes(cell);
                                    let at = self.buf.pos();
                                    self.buf.emit(&bytes);
                                    exact_hit.extend(hits.into_iter().map(|h| at + h));
                                    for e in edges {
                                        self.patch_rel32_to_here(at + e);
                                    }
                                    // tca21 review fix: the display arm's declines
                                    // arrive with RDX holding the value's DISPLAY
                                    // ADDRESS (`MOV RDX, [R10 + RDX*8 + 8]`), but
                                    // the row arm below reads EDX as the value's
                                    // class id (a pointer whose low half is under
                                    // `ROWS_CAP` would index another class's row
                                    // and could pass a store the helper refuses).
                                    // RAX (the value) and R11 (the array) survive
                                    // the display arm, so reload the id.
                                    self.buf.emit(&[0x8B, 0x10]); // MOV EDX, [RAX]
                                }

                                self.emit_aastore_subtype_arm(&mut exact_hit); // TCARRAY21-2
                                self.patch_rel32_to_here(not_plain);
                            }
                            // With the gates, the check maps on its own path:
                            // FIRST, before the arguments, because the shadow
                            // push clobbers scratch registers.
                            let mut check_map = if map_on_helper_path {
                                self.emit_safepoint_map_if_enabled()
                            } else {
                                None
                            };
                            // (vm_ptr, array_ptr, val) -> 0 | i64::MIN.
                            self.load_context(CALL_ARG_REGS[0]);
                            self.gp_load_value(CALL_ARG_REGS[1], node.inputs[2]);
                            self.gp_load_value(CALL_ARG_REGS[2], node.inputs[4]);
                            self.emit_mov_reg_imm64(RAX, self.aastore_type_check as u64);
                            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                            self.anchor_safepoint_return(
                                check_map.as_mut().or(map.as_mut()),
                                self.buf.pos(),
                            );
                                                          // The refusal builds its throwable in the helper,
                                                          // whose stack walk passes this return address.
                            self.note_inline_frame_return_site();
                            if map.is_some() || check_map.is_some() {
                                // RAX (the answer) survives; flags do not, so
                                // the compare comes after.
                                self.emit_pending_shadow_copy_back();
                            }
                            // `CMP RAX, 1 ; JO → exception epilogue`. `Int`: the
                            // helper answers only `0` or the sentinel, so the
                            // plain compare is unambiguous. The refusal exit
                            // leaves before the reload below, as every call
                            // exception exit does (the epilogue restores `top`).
                            self.emit_call_return_sentinel_tail(IrType::Int);
                            if check_map.is_some() {
                                // Retract before the join: the null and exact
                                // exits arrive with nothing pushed.
                                self.emit_shadow_reload();
                            }
                            self.patch_rel32_to_here(null_value);
                            for hit in exact_hit {
                                self.patch_rel32_to_here(hit);
                            }
                        }
                        let fresh_receiver = self.zgc_fresh_stores.contains(&id);
                        let inline_done: Vec<usize> = match (inline_gates, g1_inline) {
                            (Some(gates), _) => self.emit_gated_ir_aastore(
                                gates,
                                node.inputs[2],
                                node.inputs[3],
                                node.inputs[4],
                                fresh_receiver,
                            ),
                            (None, Some(plan)) => self.emit_g1_ir_aastore(
                                plan,
                                node.inputs[2],
                                node.inputs[3],
                                node.inputs[4],
                            ),
                            (None, None) => Vec::new(),
                        };
                        self.emit_ref_array_store_helper_fallback(
                            node.inputs[2],
                            node.inputs[3],
                            node.inputs[4],
                            inline_done,
                            map,
                            map_on_helper_path,
                        );
                        return;
                    }
                    let value = node.inputs[4];
                    let has_implicit_plan = self.implicit_throw_plans.contains_key(&id);
                    let select_source = !has_implicit_plan
                        && self.mir_mode == MirMode::Off
                        && !self.carry_names(value)
                        && ((*kind == MemKind::Int && self.operand_in_place_on)
                            || (self.reg_alloc_on
                                && matches!(
                                    kind,
                                    MemKind::Long | MemKind::Byte | MemKind::Char | MemKind::Short
                                )));
                    // The ordinary guard path preserves callee-saved and caller-saved
                    // residency registers. Its failure pads never rejoin the store.
                    let operand = if select_source {
                        self.resident_gpr(value)
                            .filter(|r| {
                                IR_LOWER_LS_GPRS.contains(r)
                                    || (self.reg_alloc_on && IR_LS_CALLER_SAVED_GPRS.contains(r))
                            })
                            .map(|r| {
                                if self.reg_alloc_on {
                                    self.gp_reg_reads += 1;
                                }
                                ArrayStoreSource::Reg(r)
                            })
                            .or_else(|| {
                                if *kind == MemKind::Int && self.const_imm_on {
                                    self.alu_imm32(value).map(ArrayStoreSource::IntImm)
                                } else {
                                    None
                                }
                            })
                    } else {
                        None
                    };
                    let mut operand = operand.unwrap_or_else(|| {
                        self.gp_load_value(RDX, value);
                        ArrayStoreSource::Reg(RDX)
                    });
                    let array = node.inputs[2];
                    let resident_base = if self.reg_alloc_on
                        && !has_implicit_plan
                        && crate::gpu_barrier::barrier_bytes().is_none()
                        && !self.carry_names(array)
                    {
                        self.resident_gpr(array)
                    } else {
                        None
                    };
                    let base = match resident_base {
                        Some(base) => {
                            self.gp_reg_reads += 1;
                            base
                        }
                        None => {
                            self.gp_load_value(RAX, array); // array → RAX
                            RAX
                        }
                    };
                    let index = self.load_array_index(node.inputs[3], !has_implicit_plan);
                    self.emit_array_null_bounds_guards_at(bci, id, base, index);
                    if matches!(kind, MemKind::Byte)
                        && !self.int_value_is_boolean_bits(node.inputs[4])
                        && !self.array_known_non_boolean(array)
                    {
                        // Round 9 wave 8 (irl8): JVMS 6.5 `bastore` narrows by
                        // `value & 1` into a `boolean[]` and truncates to a byte
                        // into a `byte[]`; the opcode serves both and nothing
                        // here knows which. The interpreter masks
                        // (`write_prim_element`'s `Boolean` arm), so without
                        // this `iconst_2; bastore` into a `boolean[]` stored 2
                        // in compiled code and 0 interpreted. Same decision
                        // as the single-pass `0x54` arm: ask the header's
                        // kind byte, which the bounds check's length load has
                        // just brought into cache.
                        let Some(bool_tag) = cratonvm_types::primitive_array_kind_tags_byte("[Z")
                        else {
                            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                                "ir_lower: no kind byte for boolean[]",
                            )));
                            return;
                        };
                        if let ArrayStoreSource::Reg(src_reg) = operand {
                            if src_reg != RDX {
                                self.emit_mov_reg_reg64(RDX, src_reg);
                                operand = ArrayStoreSource::Reg(RDX);
                            }
                        }
                        if base == RAX {
                            self.buf.emit(&[
                                0x80,
                                0x78,
                                cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                                bool_tag,
                            ]); // CMP BYTE [RAX+KIND_TAGS_BYTE_OFFSET], tag("[Z")
                        } else {
                            use crate::x64::isel::{select, Mem, Op as MachineOp, Operand, Req, Ty};
                            match select(&Req::new(
                                MachineOp::Cmp,
                                Ty::I8,
                                Operand::Mem(Mem::base_disp(
                                    base,
                                    cratonvm_types::KIND_TAGS_BYTE_OFFSET as i64,
                                )),
                                Operand::Imm(i64::from(bool_tag)),
                            )) {
                                Ok(cmp) => self.buf.emit(&cmp.encoded.bytes),
                                Err(_) => {
                                    self.latch_bailout(Bailout::new(BailoutReason::Internal(
                                        "ir_lower: boolean[] tag compare has no encoding",
                                    )));
                                    return;
                                }
                            }
                        }
                        self.buf.emit(&[0x75, 0x03]); // JNE +3 (a byte[]: store DL as is)
                        self.buf.emit(&[0x83, 0xE2, 0x01]); // AND EDX, 1
                    }
                    self.emit_gpr_array_elem_store_at(*kind, base, index, operand);
                    // RAX still holds the array pointer -- the store above
                    // addresses through it. See `crate::gpu_barrier`.
                    self.emit_gpu_input_cache_barrier();
                    return;
                }
                let is_d = matches!(kind, MemKind::Double);
                self.fp_load_value(XMM0, node.inputs[4], is_d); // value → XMM0
                let (base, index) = self.bind_checked_array_load_address(id, node, bci);
                self.emit_fp_array_elem_access(is_d, true, base, index);
                if crate::gpu_barrier::barrier_bytes().is_some() && base != RAX {
                    self.emit_mov_reg_reg64(RAX, base);
                }
                // Same contract as the integral arm above: RAX is the array.
                // The barrier clobbers R10/R11 and the flags only, so an
                // XMM-resident value elsewhere in the frame is untouched.
                self.emit_gpu_input_cache_barrier();
            }
            // arraylength (COV-02). inputs = [ctrl, mem, array]. One 32-bit
            // load at a fixed header offset behind the JVMS null check. No
            // element type, no bounds check, no barrier — the cheapest node in
            // this lane and 43 of its 77 measured events.
            //
            // `MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]` zero-extends into RAX,
            // which is also the correct sign extension: an array length is a
            // non-negative `u32` bounded by `i32::MAX`. Byte-identical to the
            // single-pass `emit_arraylength_regs`.
            //
            // The null path DEOPTS rather than jumping over the load: control
            // leaves for the shared stub, the interpreter re-executes this
            // `arraylength` and throws the real NullPointerException with the
            // method's own handler semantics. Emitting the load without the
            // check would be a SIGSEGV in generated code.
            Op::ArrayLength => {
                let slot = self.alloc_slot(id);
                let bci = self.deopt_pc_of(node);
                if self.lower_allocated_array_length(id, node.inputs[2], slot, bci) {
                    return;
                }
                self.gp_load_value(RAX, node.inputs[2]); // array → RAX
                // Round 11 (irlower): `ir_check_elim` lists `ArrayLength` as a
                // dereference and computes its null fact like the element
                // accesses' -- a fresh array, or one a dominating access already
                // dereferenced -- but only `ArrayLoad`/`ArrayStore` consulted
                // it, so `a.length` re-tested a proven array on every
                // evaluation (javac re-reads it at every loop test). Counted
                // either way, as the element arms count theirs.
                let elide_null = self.check_elision.null_elided(id);
                crate::ir_check_elim::note_check(0, elide_null);
                if !elide_null {
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
                }
                let direct_result = if node.ty == IrType::Int {
                    self.integer_result_gpr(node.ty, slot)
                } else {
                    None
                };
                let disp = crate::x64::disp::disp8_const(ARRAY_LENGTH_OFFSET as i64) as u8;
                if let Some(dst) = direct_result {
                    let rex = 0x40 | (u8::from(dst >= 8) << 2);
                    if rex != 0x40 {
                        self.buf.emit_byte(rex);
                    }
                    self.buf.emit(&[0x8B, 0x40 | ((dst & 7) << 3), disp]);
                    self.publish_integer_result(dst, slot);
                } else {
                    // MOV EAX, [RAX + ARRAY_LENGTH_OFFSET].
                    self.buf.emit(&[0x8B, 0x40, disp]);
                    self.store_rax(slot);
                }
            }
            // Round 9: `jit_lambda_int_to_double` runs a compiled or
            // interpreted Java `get(I)D`, so it can allocate, collect and throw
            // (it returns `i64::MIN` under its panic guard). The arm used to be
            // a bare CALL: no oop map (the lambda reference unpublished, the
            // sp-id slot naming an EARLIER safepoint), no post-call frame
            // record or shadow reload, and no sentinel check — a thrown
            // exception became the double `-0.0`. It now has the `Op::Call` /
            // `Op::LoadStatic` helper-route obligations: map before
            // `alloc_slot`, arguments after the map (the shadow push clobbers
            // the argument registers), and `emit_call_return_check` with the
            // `Double` width's `dispatch_threw` disambiguation. Matches the
            // single-pass twin (`x64/op_invoke.rs`), which publishes a map and
            // runs `emit_post_invoke_exception_check(b'D')`.
            Op::LambdaIntToDouble => {
                if self.dispatch_threw == 0 {
                    // Only a synthetic helper table leaves this zero; the wide
                    // sentinel check would CALL address 0 on its cold path.
                    self.latch_bailout(Bailout::new(BailoutReason::UnsupportedShape(
                        "ir_lower: LambdaIntToDouble needs dispatch_threw",
                    )));
                    return;
                }
                let sp_live_hi = self.spill_high_water;
                let mut map = self.emit_safepoint_map(sp_live_hi);
                let slot = self.alloc_slot(id);
                self.load_context(CALL_ARG_REGS[0]);
                self.gp_load_value(CALL_ARG_REGS[1], node.inputs[2]);
                self.gp_load_value(CALL_ARG_REGS[2], node.inputs[3]);
                self.emit_mov_reg_imm64(RAX, self.lambda_int_to_double as u64);
                self.buf.emit(&[0xFF, 0xD0]);
                self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
                self.emit_call_return_check(slot, IrType::Double);
                self.publish_fp_from_slot(id, slot, true);
            }
            // Round 16 (lane indy, SY4-1): a BRIDGED `invokedynamic`
            // (`CRATONVM_JIT_IR_INDY_BRIDGE`). inputs = [ctrl, mem, arg0, …].
            //
            // The generic `Op::Call` dispatch route below with one word
            // changed: `jit_indy_bridge(vm, site, args_ptr, n)` has
            // `jit_invoke_dispatch`'s ABI exactly, and it is the entry the
            // single-pass `0xba` arm calls (`DirectHelperTable::indy_bridge`), so
            // the two tiers share one helper and one site pointer. The same
            // obligations, in the same order:
            //   * the map FIRST, before `alloc_slot` (the result word is not
            //     written until the call returns) and before the marshal (the
            //     shadow push calls `get_current_thread`, which clobbers the
            //     argument registers). The bridge decodes every staged word
            //     into a pinned `Value` before it can allocate, so the staged
            //     copies need no map entry of their own;
            //   * the shared post-call sequence (`emit_call_return_check`):
            //     frame republish, shadow reload, and the `i64::MIN` sentinel
            //     edge at this bci -- a throwing operand `toString()` or a
            //     `BootstrapMethodError` is stashed by the bridge and leaves
            //     through `push_call_exc_patch`, i.e. the protected-range
            //     rethrow pad or the propagating exit, like any call.
            Op::IndyBridge { site } => {
                let bridge = self.direct_helpers.indy_bridge;
                let num_args = node.inputs.len().saturating_sub(2);
                let wide = matches!(node.ty, IrType::Long | IrType::Double | IrType::Float);
                if bridge == 0
                    || *site == 0
                    || node.ty == IrType::Void
                    || num_args > self.args_stage_words
                    || (wide && self.dispatch_threw == 0)
                {
                    // A zero helper is a synthetic table (or a door that did
                    // not wire it): a CALL through address 0. A staging region
                    // narrower than the site is a `scan_frame_needs` that did
                    // not count this op. Neither may emit.
                    self.latch_bailout(Bailout::new(BailoutReason::UnsupportedShape(
                        "ir_lower: IndyBridge needs the bridge helper, a staging word per argument, and dispatch_threw for a wide result",
                    )));
                    return;
                }
                // JIT round 19 proposal wave 5, lane concat5 (STRGAP4-1,
                // `CRATONVM_JIT_IR_CONCAT_INLINE`, default OFF): an admitted
                // concat is built inline AHEAD of the call, which stays as its
                // decline edge. See `Self::lower_indy_bridge_concat_inline`.
                if !self.lower_indy_bridge_concat_inline(id, node, *site, bridge, num_args) {
                    // nio21 wave 10, lane L3 (W9L2-1,
                    // `CRATONVM_JIT_INDY_CONSTANT_LAMBDA`, default ON): a
                    // non-capturing lambda first reads its singleton through the
                    // site's constant word, BEFORE the map (as `Op::ConstString`'s
                    // slot probe: the map's shadow push is popped only by the
                    // reload after the CALL, so a hit must skip both):
                    //
                    //   mov rax, site ; mov rax, [rax+8] ; test ; jz miss
                    //   mov rax, [rax] ; test ; jnz hit ; miss: <bridge> ; hit:
                    //
                    // The word is the address of the instruction's
                    // `LAMBDA_SINGLETON_CACHE` slot (published by the VM at the
                    // bridge's first warm evaluation; `0` before), a GC root
                    // rewritten only while mutators are parked; a non-zero value
                    // in it is the instance the interpreter hands out. The
                    // single-pass twin is `x64/op_invoke.rs`
                    // `emit_indy_constant_probe`.
                    let constant_hit = if num_args == 0
                        && node.ty == IrType::Ref
                        && true {
                        // Cast: a process-lived site address as the imm64 operand.
                        self.emit_mov_reg_imm64(RAX, *site as u64);
                        // MOV RAX, [RAX + 8]: the VM's
                        // `JIT_INDY_SITE_CONSTANT_WORD_OFFSET`, pinned there.
                        self.buf.emit(&[0x48, 0x8B, 0x40, 0x08]);
                        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                        self.buf.emit(&[0x0F, 0x84]); // JZ miss
                        let miss_patch = self.buf.pos();
                        self.buf.emit(&[0; 4]);
                        self.buf.emit(&[0x48, 0x8B, 0x00]); // MOV RAX, [RAX]
                        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                        self.buf.emit(&[0x0F, 0x85]); // JNZ hit
                        let hit_patch = self.buf.pos();
                        self.buf.emit(&[0; 4]);
                        let miss = self.buf.pos();
                        let rel = miss as i32 - (miss_patch as i32 + 4);
                        Self::patch_or_bail(&mut self.buf, miss_patch, rel);
                        Some(hit_patch)
                    } else {
                        None
                    };
                    let sp_live_hi = self.spill_high_water;
                    let mut map = self.emit_safepoint_map(sp_live_hi);
                    let slot = self.alloc_slot(id);
                    for i in 0..num_args {
                        let arg = node.inputs[2 + i];
                        self.gp_load_value(RAX, arg);
                        self.store_rax(self.args_stage_top_off - (i as i32) * 8);
                    }
                    self.load_context(CALL_ARG_REGS[0]); // vm_ptr
                    self.emit_mov_reg_imm64(CALL_ARG_REGS[1], *site as u64); // site_ptr
                    self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off); // args_ptr
                    self.emit_mov_reg_imm64(CALL_ARG_REGS[3], num_args as u64); // arg_count
                    self.emit_mov_reg_imm64(RAX, bridge as u64);
                    self.buf.emit(&[0xFF, 0xD0]);
                    self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
                    if constant_hit.is_some() {
                        // `emit_call_return_check` with the hit landing between
                        // the sentinel tail and the result store: both arms hold
                        // the reference in RAX there.
                        self.note_inline_frame_return_site();
                        self.emit_post_call_frame_record();
                        self.emit_shadow_reload();
                        self.emit_call_return_sentinel_tail(node.ty);
                        self.patch_ldc_slot_hit(constant_hit);
                        self.store_rax(slot);
                    } else {
                        self.emit_call_return_check(slot, node.ty);
                        if self.graph.sb_chain_fused_sites.contains(site) {
                            if let Some(bci) = node.bytecode_pc {
                                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                                self.emit_deopt_if_zero(bci, DeoptReason::UncommonTrap);
                            }
                        }
                    }
                    if matches!(node.ty, IrType::Double | IrType::Float) {
                        self.publish_fp_from_slot(id, slot, node.ty == IrType::Double);
                    }
                }
            }
            // Round 16 (lane matrix): `multianewarray`
            // (`CRATONVM_JIT_IR_MULTIANEWARRAY`). inputs = [ctrl, mem, dim0, …].
            //
            // `jit_multianewarray_n(vm, site, ndims, dims_ptr)` -- the helper
            // the single-pass `0xc5` arm calls, with the same packed site word
            // -- given the dimensions staged in the call-argument region exactly
            // as `Op::IndyBridge` stages its arguments: `args_ptr[i]` is input
            // `2 + i`, outermost first, which is the order the helper reads its
            // buffer in. The same obligations, in the same order:
            //   * the map FIRST, before `alloc_slot` and before the staging
            //     (resolution can run a user class loader and the allocation
            //     can collect; the staged words are ints, not oops);
            //   * after the CALL: the inline-frame row, the frame republish and
            //     the shadow reload, which `emit_call_return_check` would do;
            //   * the failure edge. NOT the `i64::MIN` sentinel: the helper
            //     answers `0` with the throwable stashed (a negative dimension,
            //     OOM, a failed resolution), so a zero result materialises the
            //     sentinel and leaves through the shared exception edge at this
            //     bci, which is `Op::NewArray`'s convention.
            Op::MultiNewArray { site } => {
                let helper = self.multianewarray_n;
                let ndims = node.inputs.len().saturating_sub(2);
                if helper == 0 || *site == 0 || ndims == 0 || ndims > self.args_stage_words {
                    // A zero helper is a synthetic table (or a door that did
                    // not wire it): a CALL through address 0. A staging region
                    // narrower than the site is a `scan_frame_needs` that did
                    // not count this op. Neither may emit.
                    self.latch_bailout(Bailout::new(BailoutReason::UnsupportedShape(
                        "ir_lower: MultiNewArray needs the multianewarray_n helper, a site and a staging word per dimension",
                    )));
                    return;
                }
                let sp_live_hi = self.spill_high_water;
                let mut map = self.emit_safepoint_map(sp_live_hi);
                let slot = self.alloc_slot(id);
                for i in 0..ndims {
                    let dim = node.inputs[2 + i];
                    self.gp_load_value(RAX, dim);
                    self.store_rax(self.args_stage_top_off - (i as i32) * 8);
                }
                self.load_context(CALL_ARG_REGS[0]); // vm_ptr
                self.emit_mov_reg_imm64(CALL_ARG_REGS[1], *site as u64); // site
                self.emit_mov_reg_imm64(CALL_ARG_REGS[2], ndims as u64); // ndims
                self.lea_reg_from_frame(CALL_ARG_REGS[3], self.args_stage_top_off); // dims_ptr
                cratonvm_jit_api::assert_helper_call_shape!(
                    "multianewarray_n",
                    int_args = 4,
                    returns_value = true
                );
                self.emit_mov_reg_imm64(RAX, helper as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
                self.note_inline_frame_return_site();
                self.emit_post_call_frame_record();
                self.emit_shadow_reload();
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
                self.store_rax(slot);
            }
            // nio21 wave 15 (lane L3; `CRATONVM_JIT_IR_DEFERRED_NEW`): a `new`
            // whose class was not loaded at compile time. inputs = [ctrl, mem].
            //
            // `jit_new_object_cp(vm, holder, cp_idx)` through the shared
            // `emit_new_object_cp_stub` -- the helper, ABI and emitter of the
            // single-pass `0xbb` deferred arm, and the `(vm, holder, cp_idx)`
            // sequence `Op::ConstClass` calls `jit_ldc_class_cp` with. The same
            // obligations as that arm, in the same order: the map FIRST
            // (resolution can run a user class loader and `<clinit>`, the
            // allocation can collect), then the inline-frame row at the return
            // address, the identity republish and the shadow reload; and the
            // failure edge -- `0` with the throwable stashed (a failed
            // resolution, `<clinit>` failure, OOM) materialises the sentinel and
            // leaves through the shared exception edge at this bci.
            Op::NewCp {
                holder_class_id,
                cp_idx,
            } => {
                let (holder_class_id, cp_idx) = (*holder_class_id, *cp_idx);
                let helper = self.new_object_cp;
                if helper == 0 {
                    self.latch_bailout(Bailout::new(BailoutReason::UnsupportedShape(
                        "ir_lower: NewCp needs the new_object_cp helper",
                    )));
                    return;
                }
                // Round rx27 wave 1 (lane alloc): a deferred `new` (its class
                // unresolved when this body compiled) calls
                // `jit_new_object_cp` on every allocation until a recompile;
                // the census names it beside the `Op::New` sites.
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!(
                        "[cratonvm-jitc] ir new-site n{id} holder={holder_class_id} cp={cp_idx} \
                         route=cp-helper call=deferred-class method={}",
                        self.dbg_method(),
                    );
                }
                let sp_live_hi = self.spill_high_water;
                let mut map = self.emit_safepoint_map(sp_live_hi);
                let slot = self.alloc_slot(id);
                let return_address = crate::runtime_lowering::emit_new_object_cp_stub(
                    &mut self.buf,
                    self.context_slot_off,
                    helper,
                    holder_class_id,
                    cp_idx,
                    self.frame_record,
                );
                self.anchor_safepoint_return(map.as_mut(), return_address);
                self.note_inline_frame_return_site_at(return_address);
                self.emit_identity_after_shared_stub();
                self.emit_shadow_reload();
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
                self.store_rax(slot);
            }
            // A Java call (Gap B). inputs = [ctrl, mem, arg0, arg1, …]. The
            // generic route dispatches via the `jit_invoke_dispatch` helper,
            // ABI (mirrors x64.rs):
            //   i64 helper(vm_ptr, info_ptr, args_ptr, num_args)
            // with the Java args marshalled into the frame staging region
            // (`args_ptr` → arg0). The self-recursive, direct and inline-cache
            // routes below replace that helper where they can. A returned
            // `i64::MIN` means the callee threw (or deopted): jump to the shared
            // bail stub, which returns the sentinel unchanged so the VM takes
            // the pending exception (the same protocol single-pass uses).
            //
            // (This comment used to say the builder emits a call "only for an
            // oop-free method, so no GC oop map is needed". Neither has been
            // true for a long time: references are live across calls and the
            // arm publishes a map first — see below.)
            Op::Call { info_ptr } => {
                // A call site the compile door found unresolved resolves at
                // its first execution, in front of every route below
                // (interpreter round i1 wave 50, lane L1;
                // `crate::JitFirstCallCell`). Register-transparent and a leaf,
                // so no map, reload or republish follows it.
                if let Some(&(_, cell)) = self
                    .graph
                    .first_call_cells
                    .iter()
                    .find(|&&(info, _)| info == *info_ptr)
                {
                    crate::runtime_lowering::emit_first_call_guard(&mut self.buf, cell);
                    if false {
                        eprintln!(
                            "[first-call] optimizing guard bci={:?} cell={cell:#x}",
                            node.bytecode_pc
                        );
                    }
                }
                // Round 9 wave 9 (irl9): the inline `ArrayList.get`/`size`
                // prefix, ahead of every byte of the call lowering below (its
                // decline edges land on that lowering, unchanged). Emits
                // nothing unless this compile carries an ArrayList layout.
                self.try_emit_ir_arraylist_access_prefix(id, *info_ptr);
                // Round rx27 wave 3 (lane splitfast): the inline
                // `ArrayList.add(Object)` prefix, under its opt-in switch.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_arraylist_add_prefix(id, *info_ptr);
                }
                // Round 11 wave 12 (lane hashmap's page
                // `r11w12-hashmap-ir-box-hash-equals-prefix-patch`, applied by
                // lane calls): the `Integer.hashCode()` /
                // `Integer.equals(Object)` prefix, the twin of the single-pass
                // `try_emit_box_hash_equals_prefix`. Its method names are
                // disjoint from the ArrayList prefix's; asked only when that
                // one did not claim the node.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_box_hash_equals_prefix(id, *info_ptr);
                }
                // Round 11 wave 17 (lane hashmap, proposal W15-1): the inline
                // out-of-cache `Integer.valueOf` box. A static site, so it is
                // disjoint from both prefixes above.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_integer_value_of_prefix(id, *info_ptr);
                }
                // Round 12 wave 2 (lane calls, proposal C12-2): its
                // `Long.valueOf` twin; the method triples are disjoint.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_long_value_of_prefix(id, *info_ptr);
                }
                // nio20 wave 1, lane L3 (NIO19-5): `getClass()` as two loads
                // from the VM's compiled-`getClass()` table; `getClass` is
                // disjoint from every prefix above.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_getclass_prefix(id, *info_ptr);
                }
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_reference_get0_prefix(id, *info_ptr);
                }
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_class_mirror_fast_prefix(id, *info_ptr);
                }
                // nio20 wave 1, lane L2 (NIO19-2): an `Unsafe` primitive
                // accessor over a heap primitive array, as one load or store;
                // `jdk/internal/misc/Unsafe` sites are disjoint from every
                // prefix above.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_unsafe_access_prefix(id, *info_ptr);
                }
                // nio20 wave 3, lane L2: `String.hashCode` / `equals` /
                // `compareTo` / both `indexOf` on a `java/lang/String`-declared
                // site; the box prefix above claims only `Object` / `Number` /
                // `Integer`-declared sites, so the triples are disjoint.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_string_search_prefix(id, *info_ptr);
                }
                // JIT r23 wave 6, lane L5: a barrier-free, check-free
                // reference `System.arraycopy` copied inline; the one triple
                // is disjoint from every prefix above.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_ref_arraycopy_prefix(id, *info_ptr);
                }
                // JIT r23 wave 8, lane L8: `Array.newArray` allocated inline;
                // the one triple is disjoint from every prefix above.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_reflect_new_array_prefix(id, *info_ptr);
                }
                // j27 wave 4, lane CLONEIR: an array `clone()` bound to a thin
                // clone helper, allocated and copied inline; a virtual
                // `[..].clone()` triple, disjoint from every prefix above.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_array_clone_prefix(id, *info_ptr);
                }
                // nio20 wave 5, lane L2: `System.identityHashCode` and a
                // `java/lang/Class`-declared `hashCode()` from the header; both
                // triples are disjoint from every prefix above (the box prefix
                // takes `Object`-declared `hashCode`, and carries its own
                // `Class` arm there).
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_identity_hash_prefix(id, *info_ptr);
                }
                // nio21 wave 1, lane L5: `Thread.currentThread()` from the
                // per-VM thread word (`--compatible`); a static triple disjoint
                // from every prefix above.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_current_thread_prefix(id, *info_ptr);
                }
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_byte_array_view_prefix(id, *info_ptr);
                }
                // nio21 wave 11 L5 (W9L2-2): a constant-handle `VarHandle`
                // field access; `java/lang/invoke/VarHandle` sites are
                // disjoint from every prefix above.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_vh_const_prefix(id, *info_ptr);
                }
                // JIT round 22 (lane memory, FFM22-1): a constant bound FFM
                // downcall's `invokeExact` made as the C call itself; a
                // `MethodHandle.invokeExact` triple, disjoint from every
                // prefix above (the `VarHandle` one takes `VarHandle` sites).
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_ffm_const_prefix(id, *info_ptr);
                }
                // JIT round 20 wave 17, lane pAtom2: `Class.isInstance` of an
                // exact-class object from the compiled-`getClass()` table; a
                // `java/lang/Class` `isInstance` triple, disjoint from every
                // prefix above.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_class_is_instance_prefix(id, *info_ptr);
                }
                // JIT round 20 wave 19, lane zAssign: `Class.isAssignableFrom`
                // from the mirror hints and the supers displays; a
                // `java/lang/Class` `isAssignableFrom` triple, disjoint from
                // every prefix above.
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_class_is_assignable_prefix(id, *info_ptr);
                }
                // JIT round 21 wave 16, lane perf-cf-lean
                // (`CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD`, opt-in): a `void`
                // call whose target returns at once while a field of `this`
                // is null is skipped then. Asked last: every prefix above
                // claims a JDK triple this one's VM answer never admits (none
                // is a pure null-test-and-return entry).
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_off_heap_copy_prefix(id, *info_ptr);
                }
                if self.call_prefix_join.is_none() {
                    self.try_emit_ir_call_entry_null_guard_prefix(id, *info_ptr);
                }
                // nio20 wave 6, lane L3: a fired prefix is acceptance evidence
                // under `CRATONVM_JIT_IR_EVIDENCE_CALL_PREFIX` (opt-in;
                // recorded always, `ir_evidence::note_call_prefix`).
                if self.call_prefix_join.is_some() {
                    crate::ir_evidence::note_call_prefix();
                }
                // hga: the planned dead-home clears of a prefixed call, at the
                // head of its call path (every decline edge lands here).
                self.emit_pending_call_clears();
                // nio20 wave 1, lane L2 (NIO19-9): `Reference.reachabilityFence`
                // emits no code. The node stays in the graph, so its operand
                // stays live -- in its register or home, and in every map --
                // up to here, which is the whole of the method's contract.
                if self.call_prefix_join.is_none() && self.try_lower_ir_reachability_fence(id, *info_ptr)
                {
                    return;
                }
                // j27 wave 5 (lane JMMHUNT): `Thread.onSpinWait()` is one
                // `PAUSE`; the node stays the compiler barrier C2's `MemBar`
                // is (`ir_lower::on_spin_wait`).
                if self.call_prefix_join.is_none() && self.try_lower_ir_on_spin_wait(id, *info_ptr) {
                    return;
                }
                // Relocation contract: publish the map for this GC-capable
                // point BEFORE `alloc_slot`. Every route out of this arm can
                // reach a collection, and `alloc_slot` marks the call's own
                // result slot defined — but that slot is not written until the
                // call RETURNS, so covering it here would publish whatever the
                // previous frame left at that offset as a live reference.
                // `live_frame_hi` is the spill watermark as it stands now, i.e.
                // before this node's result slot is carved. Under liveness
                // colouring the call's result slot may *recycle* a colour, so
                // "the watermark" is no longer "one past the last slot handed
                // out" — but it is still an upper bound on every word this
                // frame has written, which is exactly what the band scan needs.
                let sp_live_hi = self.spill_high_water;
                // Round 11 wave 15 (lane fib; step 3 of
                // `r11w14-rt-ir-lower-oop-free-frame-patch`): a direct
                // self-call in a body that keeps no reference records its map
                // but stores the id only on `emit_self_recursive_call`'s cold
                // paths. Below the innermost frame such a frame is placed by
                // its return address, and the collector never reads its id.
                self.defer_next_sp_id_store = Self::is_direct_self_call(node)
                    && !self.self_call_receiver_needs_check(node)
                    && self.body_publishes_no_roots();
                let mut map = self.emit_safepoint_map(sp_live_hi);
                self.defer_next_sp_id_store = false;
                // nio21 wave 3 L2: from here on the frame word names THIS
                // call's map, so an armed site's exceptional edges may run the
                // GC-capable handler lookup (`push_call_exc_patch`).
                self.call_throw_publication = Some(CallThrowPublication {
                    map: map.as_ref().filter(|map| map.id_stored).cloned(),
                });
                let slot = self.alloc_slot(id);
                let num_args = node.inputs.len().saturating_sub(2);
                // JIT round 25 wave 7 (W7-C): this call's hot-path frame
                // republish moves to its sentinel side when nothing after it
                // reads the frame record (`plan_tail_republish_sink`). Not for
                // a prefixed site, whose prefix may emit calls of its own.
                self.tail_sink.cur = self.call_prefix_join.is_none() && self.tail_sink.admits(id);
                // fib44-fix follow-up: invoke_kind 4 marks a self-recursive call
                // the eligibility loop chose to emit as a DIRECT call to this
                // method's own entry (CRATONVM_JIT_IR_SELFREC_DIRECT), bypassing
                // the generic `jit_invoke_dispatch` helper. SAFETY: `info_ptr`
                // points to a live `JitInvokeInfo` owned by `ir_call_infos` for
                // the whole compile. An early `return` here skips only the generic
                // register publish after `lower_data_node`'s `match`; the route's own
                // `store_rax` publishes (`publish_def_at_store`), and with that off
                // the value is read from its home word (round 11: this said there
                // was no post-`match` code).
                if unsafe { (*(*info_ptr as *const JitInvokeInfo)).invoke_kind } == 4 {
                    // This route bypasses `emit_call_return_check`, so the
                    // shared post-call reload never runs here; it is emitted
                    // explicitly below.
                    //
                    // The map published above KEEPS its coverage claim (round
                    // 11 wave 5, lane lower; it was withdrawn here until then,
                    // `r11-irlower-self-recursive-call-withdraws-moving-coverage`).
                    // The route discharges every obligation the shared routes
                    // discharge through `emit_call_return_check`, in the same
                    // order as `emit_direct_cross_call`'s non-merged branch,
                    // which has always kept the claim:
                    //   * the sampled stack-guard CALL, the one call between the
                    //     push and the recursive CALL, copies the published
                    //     words back into the homes before the argument marshal
                    //     reads them (`emit_pending_shadow_copy_back`, wave 4);
                    //   * after the CALL: inline-frame row, RBP republish, the
                    //     identity on the sentinel path, and the callee-deopt
                    //     service with `after_reload = false`, which copies back
                    //     on its own cold side before it can collect;
                    //   * nothing between the CALL and the reload below reads a
                    //     home: `dispatch_threw` reads a flag and cannot reach a
                    //     safepoint, and `store_rax(slot)` writes the result
                    //     word, which `plan_slots` makes `fresh_only` for a `Ref`
                    //     call result (never a colour this map named) and which
                    //     is in the disjoint `Prim` pool otherwise.
                    // Stress witness: `R11LowerSelfRecGc` under a moving young
                    // collector; `CRATONVM_JIT_IR_SELFREC_DIRECT=0` removes the
                    // route altogether.
                    //
                    // Dropping `pending_shadow` instead (the original) withdrew
                    // the claim but left the PUSH that `emit_safepoint_map`
                    // above had already emitted, with nothing to pop it. Every
                    // execution of a self-recursive direct call then leaked its
                    // published oops permanently, and a recursive method in a
                    // hot loop walked the thread's 2 MiB shadow stack off its
                    // end within a second — storing across the heap behind it,
                    // because no backend emits the `end` guard the shadow-stack
                    // design documents. That is what made the raw JIT-to-JIT
                    // gate unsafe to open; this route only exists when it is.
                    //
                    // Round 12 wave 8 (lane callcost): whether this site's
                    // stack sample is already answered by a dominating one
                    // (`plan_self_call_sample_elision`). A call prefix that
                    // claimed a self-call would give its dominated sites a
                    // path that skips this site's sample: refused, fail
                    // closed (no prefix names a recursive method today).
                    if self.call_prefix_join.is_some() && self.self_call_sites_planned > 0 {
                        self.latch_bailout(Bailout::new(BailoutReason::Internal(
                            "ir_lower: a call prefix claimed a self-call whose sample dominates others",
                        )));
                    }
                    self.self_call_skip_sample = self
                        .self_call_sample_dominated
                        .get(id as usize)
                        .copied()
                        .unwrap_or(false);
                    // JIT round 23 wave 3d (lane L1, proxy proposal 4e;
                    // `CRATONVM_JIT_IR_DIRECT_INSTANCE_SELF_CALL`): a
                    // receiver-carrying self-call. The VM is handed the
                    // site's kind-1 twin (the callee-deopt service decodes
                    // argument 0 as the receiver only for kinds 0-2), and the
                    // receiver is null-checked first (JVMS 6.5) unless it is
                    // this method's own `this`. The check deopts to the
                    // invoke, which the interpreter re-executes and throws
                    // the canonical NPE from -- `emit_direct_cross_call`'s
                    // check. `self_call_reads_no_frame_state` keeps this bci's
                    // frame state for exactly the checked sites.
                    let service_info = self.self_call_service_info(node, *info_ptr);
                    if self.self_call_receiver_needs_check(node) {
                        let bci = self.deopt_pc_of(node);
                        self.gp_load_value(RAX, node.inputs[2]);
                        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                        self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
                    }
                    // THROWS3-3: a void / int / reference result is never the
                    // full-width sentinel (`emit_call_return_sentinel_tail`'s
                    // non-wide rule), so the lean sentinel path may leave on
                    // the service's answer alone.
                    self.self_call_result_narrow =
                        matches!(node.ty, IrType::Void | IrType::Int | IrType::Ref);
                    self.emit_self_recursive_call(
                        &node.inputs, slot, num_args, service_info, &mut map,
                    );
                    self.self_call_result_narrow = false;
                    self.emit_shadow_reload();
                    return;
                }
                // Round 9 wave 8 (irl8): a compile-time recursion cycle's
                // CLOSING edge, planned as a `JitCycleEdgeCell` site. Its
                // target is this compile's own compile-stack ancestor, so it
                // has no entry to bind yet; the cell is filled when that
                // ancestor publishes. See `emit_cycle_edge_cell_call`. A row
                // this arm cannot serve falls through to the dispatch below
                // (a cell pc never has a `direct_calls` row).
                let cell_row = node
                    .bytecode_pc
                    .and_then(|pc| self.cycle_cells.get(&pc).copied());
                if let Some(cell) = cell_row {
                    // SAFETY: as the `invoke_kind == 4` read above -- the
                    // pointer names a live `JitInvokeInfo` owned by
                    // `ir_call_infos` for the lifetime of this compile.
                    let is_static =
                        unsafe { (*(*info_ptr as *const JitInvokeInfo)).invoke_kind } == 3;
                    if cell != 0
                        && is_static
                        && crate::direct_jit_callee_calls_enabled()
                        && self.self_call_stack_guard != 0
                        && self.invoke_dispatch != 0
                        && num_args + 1 <= ENTRY_ABI_REGS.len()
                    {
                        self.emit_cycle_edge_cell_call(
                            &node.inputs,
                            slot,
                            num_args,
                            cell,
                            *info_ptr,
                            node.ty,
                            &mut map,
                        );
                        return;
                    }
                }
                // Round 11 wave 10 (lane `synccall`, page
                // `r11w9-sync-caller-held-monitor-direct-call-patch`): a
                // `static synchronized` callee CALLed directly while this frame
                // holds its class monitor (the builder's `MonitorEnter` before
                // this node, `MonitorExit` after it). Its row is under the
                // tagged key, which the plain lookup below can never reach, so
                // this is the only route that can CALL such a monitor-free
                // body. A row whose graph lost the shape, or whose window holds
                // anything that can deopt, refuses the compile: the builder's
                // monitor pair is not described by any frame state, so neither
                // a plain CALL nor the dispatch helper can stand in for it.
                if let Some(&(entry, callee_needs_ctx)) = node.bytecode_pc.and_then(|pc| {
                    self.direct_calls
                        .get(&crate::compile_request::sync_direct_row_key(pc))
                }) {
                    let monitor = if entry == 0 {
                        Err("the synchronized callee has no entry")
                    } else {
                        self.sync_direct_call_monitor(id)
                    };
                    match monitor {
                        Ok(monitor) => {
                            let monitor_slot = self.slot_of(monitor);
                            self.emit_direct_cross_call(
                                &node.inputs,
                                slot,
                                num_args,
                                entry,
                                callee_needs_ctx,
                                None,
                                *info_ptr,
                                node.ty,
                                false,
                                self.deopt_pc_of(node),
                                Some((monitor_slot, id)),
                                &mut map,
                            );
                        }
                        Err(why) => {
                            if crate::x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] ir-sync-direct-call REFUSED bci={}: {why}",
                                    self.cur_bci
                                );
                            }
                            self.latch_bailout(Bailout::with_context(
                                BailoutReason::UnsupportedShape(
                                    "caller-held synchronized direct call",
                                ),
                                why,
                            ));
                        }
                    }
                    return;
                }
                // IR direct-call lowering: a statically-bound site whose callee
                // was eagerly compiled becomes a raw `CALL` into that callee's
                // entry — no `jit_invoke_dispatch` round trip. See
                // `emit_direct_cross_call`, which now marshals arguments past
                // the entry-ABI register file onto the stack, so an over-wide
                // site is bound rather than dropped back to helper dispatch.
                //
                // Why that is sound for BOTH callee kinds this map holds:
                //
                //  * a thin `extern "C"` VM helper reads its stack arguments
                //    the way the platform C ABI says, and always could — the
                //    register-only lowering was the sole obstacle;
                //  * a JIT-compiled callee reads them too: the single-pass
                //    prologue through `emit_load_caller_arg`, and this tier's
                //    own `emit_prologue`, which loads stack-resident arguments
                //    since Gap B (the old `lower()` refusal of a graph wider
                //    than the register file is gone) — both at the offsets
                //    `stack_arg_block_size` writes them to.
                //
                // An early `return` here skips only the generic register publish
                // after `lower_data_node`'s `match` (see the self-recursive route).
                if *info_ptr != 0 && std::env::var_os("DBG_MI").is_some() {
                    // SAFETY: the planner keeps each nonzero call row alive through lowering.
                    let info = unsafe { &*(*info_ptr as *const JitInvokeInfo) };
                    if info.class_name == "java/lang/reflect/Method" && info.method_name == "invoke" {
                        eprintln!("[DBG_MI] ir_lower kind={} decl={} n={} cache={:#x} sent={:#x} mic={:#x}",
                            info.invoke_kind, info.declaring_class_id, num_args,
                            self.direct_helpers.reflect_method_invoke_cache,
                            self.direct_helpers.reflect_method_invoke_sentinel,
                            self.invoke_virtual_mic);
                    }
                }
                if self.is_method_invoke_0(*info_ptr, num_args) && self.invoke_virtual_mic != 0 {
                    self.emit_method_invoke_cache_call(
                        &node.inputs,
                        slot,
                        num_args,
                        *info_ptr,
                        node.ty,
                        &mut map,
                    );
                    return;
                }
                let direct_call_entry = node
                    .bytecode_pc
                    .and_then(|pc| self.direct_calls.get(&pc).copied())
                    .or_else(|| {
                        if *info_ptr == 0 {
                            return None;
                        }
                        // SAFETY: the planner keeps each nonzero call row alive through lowering.
                        let info = unsafe { &*(*info_ptr as *const JitInvokeInfo) };
                        if matches!(info.invoke_kind, 0 | 1)
                            && info.class_name == "java/lang/reflect/Constructor"
                            && info.method_name == "newInstance"
                            && info.descriptor == "([Ljava/lang/Object;)Ljava/lang/Object;"
                            && self.direct_helpers.ctor_new_instance != 0
                        {
                            return Some((self.direct_helpers.ctor_new_instance, true));
                        }
                        if info.invoke_kind == 3 && info.method_name == "valueOf" {
                            if info.class_name == "java/lang/Integer"
                                && info.descriptor == "(I)Ljava/lang/Integer;"
                                && self.direct_helpers.integer_value_of != 0
                            {
                                return Some((self.direct_helpers.integer_value_of, true));
                            }
                            if info.class_name == "java/lang/Long"
                                && info.descriptor == "(J)Ljava/lang/Long;"
                                && self.direct_helpers.long_value_of != 0
                            {
                                return Some((self.direct_helpers.long_value_of, true));
                            }
                        }
                        if matches!(info.invoke_kind, 0 | 1)
                            && crate::is_class_get_superclass_site(
                                info.class_name,
                                info.method_name,
                                info.descriptor,
                            )
                            && self.direct_helpers.class_get_superclass != 0
                        {
                            return Some((self.direct_helpers.class_get_superclass, true));
                        }
                        if matches!(info.invoke_kind, 0 | 1)
                            && crate::is_class_get_name_site(
                                info.class_name,
                                info.method_name,
                                info.descriptor,
                            )
                            && crate::class_get_name_direct_enabled()
                            && self.direct_helpers.class_get_name != 0
                        {
                            return Some((self.direct_helpers.class_get_name, true));
                        }
                        if matches!(info.invoke_kind, 0 | 2)
                            && crate::is_object_get_class_site(info.method_name, info.descriptor)
                            && crate::object_get_class_direct_enabled()
                            && self.direct_helpers.object_get_class != 0
                            && !self
                                .graph
                                .getclass_prefix_refused_infos
                                .contains(&(*info_ptr as usize))
                        {
                            return Some((self.direct_helpers.object_get_class, true));
                        }
                        if let Some(op) = crate::is_unsafe_accessor_op(
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                        ) {
                            if op.direct_enabled() {
                                let entry = op.helper_entry(&self.direct_helpers);
                                if entry != 0 {
                                    return Some((entry, true));
                                }
                            }
                        }
                        None
                    });
                if let Some((entry, callee_needs_ctx)) = direct_call_entry {
                    if entry != 0 {
                        // 0 = virtual, 1 = special, 2 = interface, 3 = static,
                        // 4 = self-recursive static. The first three carry a
                        // receiver in argument 0; this map holds static and
                        // special, and the test is right for all five.
                        // SAFETY: as the `invoke_kind == 4` read above — the
                        // pointer names a live `JitInvokeInfo` owned by
                        // `ir_call_infos` for the lifetime of this compile.
                        let has_receiver = matches!(
                            unsafe { (*(*info_ptr as *const JitInvokeInfo)).invoke_kind },
                            0 | 1 | 2
                        );
                        // THROWS4-1 (lane throws5): statically bound kinds
                        // only; the service's lean predicate refuses 0 / 2
                        // too, so this only saves it the question.
                        // SAFETY: as above.
                        self.direct_call_lean_site = self.direct_call_sentinel_lean_on
                            && crate::service_lean_kind_admits(unsafe {
                                (*(*info_ptr as *const JitInvokeInfo)).invoke_kind
                            });
                        // JIT round 21, lane argwin (OOM21-1, default ON): the
                        // owned-arguments hand-off, and its callee window.
                        // Both `false` / empty off their switches.
                        let call_pc = self.deopt_pc_of(node);
                        self.direct_call_args_owned =
                            self.ir_direct_call_args_owned_by_callee(entry, *info_ptr, call_pc);
                        self.direct_call_arg_window = if self.direct_call_args_owned {
                            self.owned_call_arg_window(id, &node.inputs, num_args, call_pc)
                        } else {
                            Vec::new()
                        };
                        // gkr (OOM21-2): a refused site's argument-only homes,
                        // which only the callee-deopt service reads after the
                        // CALL, for the collector's re-run window.
                        self.direct_call_service_copies = if !self.direct_call_args_owned
                            && crate::service_copy_window_enabled()
                            && self.ir_direct_call_site_admits_service_copy_window(
                                *info_ptr, call_pc,
                            ) {
                            self.argument_only_homes(id, &node.inputs, num_args, call_pc)
                        } else {
                            Vec::new()
                        };
                        self.emit_direct_cross_call(
                            &node.inputs,
                            slot,
                            num_args,
                            entry,
                            callee_needs_ctx,
                            self.graph.native_call_contexts.get(info_ptr).copied(),
                            *info_ptr,
                            node.ty,
                            has_receiver,
                            call_pc,
                            None,
                            &mut map,
                        );
                        self.direct_call_lean_site = false;
                        self.direct_call_args_owned = false;
                        self.direct_call_arg_window.clear();
                        self.direct_call_service_copies.clear();
                        return;
                    }
                }
                // JIT round 22 (lane memory, FFM22-1 step (b)): a NON-critical
                // constant bound FFM downcall calls its row's thin helper
                // (`emit_ffm_lazy_call`), ahead of the inline cache whose
                // miss path would reach the same handle through the door.
                // Step (c): a row only its helper can call (segment
                // arguments, an object return), the same route.
                if (ir_ffm_lazy_call_enabled() || ir_ffm_segment_call_enabled())
                    && self.invoke_dispatch != 0
                {
                    if let Some((row_addr, row)) = self.ffm_const_site(id, *info_ptr) {
                        let wanted = if row.flags & FFM_CONST_HELPER_ONLY != 0 {
                            ir_ffm_segment_call_enabled()
                        } else {
                            row.flags & FFM_CONST_CRITICAL == 0 && ir_ffm_lazy_call_enabled()
                        };
                        // FFM22-3: a row with one segment parameter the
                        // compiled caller checks itself, when the stage has
                        // the extra words (`scan_frame_needs`).
                        let seg_inline = (wanted
                            && row.flags & FFM_CONST_HELPER_ONLY != 0
                            && row.call_helper != 0
                            && row.seg_call_helper != 0
                            && row.seg_param < row.arity
                            && num_args + FFM_SEG_STAGE_EXTRA <= self.args_stage_words
                            && ir_ffm_segment_inline_enabled())
                        .then(|| ffm_segment_inline_layout(&self.direct_helpers))
                        .flatten();
                        if let Some(lay) = seg_inline {
                            self.emit_ffm_seg_call(
                                &node.inputs,
                                slot,
                                num_args,
                                row_addr,
                                &row,
                                *info_ptr,
                                node.ty,
                                lay,
                                &mut map,
                            );
                            return;
                        }
                        if wanted && row.call_helper != 0 {
                            self.emit_ffm_lazy_call(
                                &node.inputs,
                                slot,
                                num_args,
                                row_addr,
                                &row,
                                *info_ptr,
                                node.ty,
                                &mut map,
                            );
                            return;
                        }
                    }
                }
                // IR inline-cache lowering (jit-inlining-and-ir-calls): a
                // virtual / interface site with allocated MIC + PIC slots gets
                // the same monomorphic-then-polymorphic cascade the single-pass
                // backend emits, falling back to `jit_invoke_virtual_mic` (which
                // populates both caches) rather than the blind
                // `jit_invoke_dispatch`. Round 11 wave 13 (lane `icwide`): a
                // site whose arguments overflow the entry-ABI register file by
                // up to `ir_ic_max_stack_words()` is admitted too, and its hits
                // marshal the overflow onto the stack (`ir_ic_admits_arity`,
                // the planner's own gate). Wider sites fall through to the
                // unchanged helper dispatch below. An early `return` skips only
                // the generic register publish after the `match` (see above).
                // The inline cascade, including its hashed megamorphic tail,
                // calls compiled entries directly.  Keep it under the same
                // master switch as the baseline backend: the opt-out must
                // route every virtual call through the helper, not merely
                // disable the MIC/PIC prefix while leaving the megamorphic
                // raw-call stub reachable.
                let is_virtual_kind = *info_ptr != 0
                    && ir_ic_cascade_admits_kind(
                        // SAFETY: the planner keeps each nonzero call row alive through lowering.
                        unsafe { (*(*info_ptr as *const JitInvokeInfo)).invoke_kind },
                    );
                if crate::direct_jit_callee_calls_enabled() && is_virtual_kind {
                    if let Some(&(mic, pic)) =
                        node.bytecode_pc.and_then(|pc| self.ic_slots.get(&pc))
                    {
                        if mic != 0
                            && pic != 0
                            && self.invoke_virtual_mic != 0
                            && ir_ic_admits_arity(num_args)
                        {
                            self.emit_inline_cache_call(
                                &node.inputs,
                                slot,
                                num_args,
                                mic,
                                pic,
                                *info_ptr,
                                node.ty,
                                &mut map,
                            );
                            return;
                        }
                    }
                }
                // Everything above returned. Reaching here means this call is
                // lowered to `jit_invoke_dispatch`, which resolves the callee
                // BY NAME on every execution -- ~175 ns against a direct
                // `CALL`'s ~4. That is sometimes the only correct answer (a
                // megamorphic site with no cache, a kind this tier cannot
                // bind), and it was silently also the answer for every
                // statically-bound call inside a SPLICED body, whose
                // `ir_direct_calls` row nothing produced. Count it, split by
                // whether the site is inside a relocated body, so that failure
                // has a reading instead of only a wall clock.
                let in_splice = node
                    .bytecode_pc
                    .is_some_and(|pc| self.pc_is_in_a_spliced_body(pc));
                note_ir_blind_dispatch(in_splice);
                // JIT r23 wave 8 (lane W8-L2, CF-W8-3): a site in a rarely
                // taken spliced arm is priced at the arm's profiled weight.
                if in_splice {
                    crate::ir_evidence::note_blind_dispatch_weight_at(node.bytecode_pc);
                }
                // The census counts these; it does not say WHICH site or why,
                // and `c2-splice-checkcast-and-instanceof-20260909.md` closes
                // on exactly that question ("that is the next thing to look
                // at, and it is a bug, not a gap") with only a count to go on.
                // Name the site and the state of every gate that could have
                // routed it here, so the answer is read rather than guessed.
                if crate::ir_stage_reporting() {
                    let pc = node.bytecode_pc.unwrap_or(usize::MAX);
                    // SAFETY: `info_ptr` is the same address this arm is
                    // about to bake into the helper call as its `info_ptr`
                    // argument; it points at a `JitInvokeInfo` owned by this
                    // compile's `owned_invoke_infos` for the artifact's life.
                    let info = unsafe { &*(*info_ptr as *const crate::JitInvokeInfo) };
                    eprintln!(
                        "[ir] blind-dispatch pc={pc} in_splice={in_splice} {}.{}{} kind={} \
                         num_args={num_args} ic_slot={} direct_row={} mic_helper={} abi_regs={} \
                         direct_calls_gate={}",
                        info.class_name,
                        info.method_name,
                        info.descriptor,
                        info.invoke_kind,
                        self.ic_slots.get(&pc).map_or("none".to_string(), |&(m, p)| format!(
                            "mic={m:#x},pic={p:#x}"
                        )),
                        self.direct_calls.contains_key(&pc),
                        self.invoke_virtual_mic != 0,
                        ENTRY_ABI_REGS.len(),
                        crate::direct_jit_callee_calls_enabled(),
                    );
                }
                // 1. Marshal each Java arg into the staging region. SBR4 osr:
                // the ABI load (an `int` sign-extended): `jit_invoke_dispatch`
                // hands a compiled callee these words as they are.
                for i in 0..num_args {
                    let arg = node.inputs[2 + i];
                    self.gp_load_abi_value(RAX, arg);
                    self.store_rax(self.args_stage_top_off - (i as i32) * 8);
                }
                // 2. Load the helper's four register arguments.
                self.load_context(CALL_ARG_REGS[0]); // vm_ptr
                self.emit_mov_reg_imm64(CALL_ARG_REGS[1], *info_ptr as u64); // info_ptr
                self.lea_reg_from_frame(CALL_ARG_REGS[2], self.args_stage_top_off); // args_ptr
                self.emit_mov_reg_imm64(CALL_ARG_REGS[3], num_args as u64); // num_args
                                                                            // 3. MOV RAX, invoke_dispatch ; CALL RAX.
                self.emit_mov_reg_imm64(RAX, self.invoke_dispatch as u64);
                self.buf.emit(&[0xFF, 0xD0]);
                self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
                // 4. Exception sentinel + result spill — shared with the direct
                //    cross-method call path; see `emit_call_return_check`.
                self.emit_call_return_check(slot, node.ty);
            }
            // ── cov-01: the constant-pool constants that are calls ───────
            //
            // `ldc <String>` and `ldc <Class>`. Both materialise a REFERENCE by
            // calling the same helper the single-pass backend calls, and both
            // publish a safepoint map first for the reason the `Op::Call` arm
            // above spells out: the helper can allocate (interning a literal,
            // constructing a mirror) and therefore collect, and the map must
            // describe the frame as it stands BEFORE this node's own result
            // slot is carved — that slot is not written until the call returns,
            // so covering it would publish whatever the previous frame left
            // there as a live reference.
            //
            // Both results take the same zero-test, because both helpers can
            // fail into a pending exception:
            //
            //   * `jit_ldc_string_cp` returns `0` with a pending exception when
            //     the literal cannot be re-read or cannot be interned on a full
            //     heap (a catchable `OutOfMemoryError` since gen r4w6). (This
            //     note used to say "`jit_ldc_string` cannot fail" -- that is the
            //     byte-literal helper, which no emitter calls any more, and it
            //     is fallible too since gen r5w2/oomjit6.)
            //   * `jit_ldc_class_cp` reports a failed resolution as `0` with a
            //     pending exception published, which is the SAME convention
            //     `Op::New` uses — so it takes the same zero-test and the same
            //     conversion to the JIT-wide `i64::MIN` before the shared
            //     exception epilogue.
            Op::ConstString {
                holder_class_id,
                cp_idx,
                slot_addr,
            } => {
                let (holder_class_id, cp_idx) = (*holder_class_id, *cp_idx);
                // Round 9 wave 8 (irl8): the GC-maintained word first, BEFORE
                // the map -- the map's shadow push is popped only by the
                // `emit_shadow_reload` after the call, so a hit that skipped
                // the call after pushing would leak one shadow frame per
                // execution. See `emit_ldc_slot_probe`.
                let slot_hit = self.emit_ldc_slot_probe(*slot_addr);
                let sp_live_hi = self.spill_high_water;
                let mut map = self.emit_safepoint_map(sp_live_hi);
                let slot = self.alloc_slot(id);
                // The same shared `(vm, holder_class_id, cp_idx)` stub the
                // `Op::ConstClass` arm below uses, for the same reason: two
                // sites that call helpers with one ABI must not each hand-roll
                // it. It also emits the post-call frame republish the helper
                // needs after crossing the JIT boundary.
                let return_address = crate::runtime_lowering::emit_ldc_class_cp_stub(
                    &mut self.buf,
                    self.context_slot_off,
                    self.ldc_string_cp,
                    holder_class_id,
                    cp_idx,
                    self.frame_record,
                );
                self.anchor_safepoint_return(map.as_mut(), return_address);
                self.note_inline_frame_return_site_at(return_address);
                self.emit_identity_after_shared_stub();
                self.emit_shadow_reload();
                // 0 = the constant-pool entry could not be re-read and a
                // pending exception was published. The bytes-baked predecessor
                // could not fail this way and so took no test; the CP-indexed
                // form can, so it takes the same test `Op::ConstClass` does.
                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
                self.buf.emit(&[0x0F, 0x85]); // JNZ resolved
                let resolved_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
                self.buf.emit_byte(0xE9); // JMP shared exception epilogue
                let exception_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.push_call_exc_patch(exception_patch);
                let resolved = self.buf.pos();
                let rel = resolved as i32 - (resolved_patch as i32 + 4);
                Self::patch_or_bail(&mut self.buf, resolved_patch, rel);
                self.patch_ldc_slot_hit(slot_hit);
                self.store_rax(slot);
            }
            // Round 15 wave 3 (lane sync2, S15-3): a `static synchronized`
            // self-locking body's monitor, its class mirror
            // (`ir::IrBuilder::set_method_self_lock_mirror`): a plain read of
            // the VM's GC-maintained mirror slot, which the VM fills from a
            // live mirror before handing it to any compile and never clears
            // (the single-pass static body reads it the same way, x64
            // `frames.rs` `emit_self_lock_refresh_mirror_word`). No helper, no
            // safepoint, no exceptional edge: it runs ahead of the body's
            // enter, where an exit would release a monitor not yet taken
            // (`Self::self_lock_refusal` relies on it).
            Op::ConstClass { slot_addr, .. } if self.self_lock_mirror_slot() == Some(*slot_addr)
                && self.self_lock.as_ref().is_some_and(|lock| lock.monitor == id) =>
            {
                let slot_addr = *slot_addr;
                let slot = self.alloc_slot(id);
                // Cast: a VM slot address as the imm64 operand.
                self.emit_mov_reg_imm64(RAX, slot_addr as u64);
                self.buf.emit(&[0x48, 0x8B, 0x00]); // MOV RAX, [RAX]
                self.store_rax(slot);
            }
            Op::ConstClass {
                holder_class_id,
                cp_idx,
                slot_addr,
            } => {
                let (holder_class_id, cp_idx) = (*holder_class_id, *cp_idx);
                // As `Op::ConstString`: the slot probe precedes the map.
                let slot_hit = self.emit_ldc_slot_probe(*slot_addr);
                let sp_live_hi = self.spill_high_water;
                let mut map = self.emit_safepoint_map(sp_live_hi);
                let slot = self.alloc_slot(id);
                // Shared with the deferred-`new` stub: `(vm, holder_class_id,
                // cp_idx)` in the entry ABI's first three argument registers,
                // an absolute CALL, then the post-call frame republish. Using
                // the shared emitter is what keeps this site's ABI from
                // drifting away from the single-pass one it mirrors.
                let return_address = crate::runtime_lowering::emit_ldc_class_cp_stub(
                    &mut self.buf,
                    self.context_slot_off,
                    self.ldc_class_cp,
                    holder_class_id,
                    cp_idx,
                    self.frame_record,
                );
                self.anchor_safepoint_return(map.as_mut(), return_address);
                self.note_inline_frame_return_site_at(return_address);
                self.emit_identity_after_shared_stub();
                self.emit_shadow_reload();
                // 0 = resolution failed and published a pending exception.
                // Convert to the JIT-wide i64::MIN and take the shared
                // exception epilogue, exactly as the `Op::New` arm does.
                self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
                self.buf.emit(&[0x0F, 0x85]); // JNZ resolved
                let resolved_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.emit_mov_reg_imm64(RAX, i64::MIN as u64);
                self.buf.emit_byte(0xE9); // JMP shared exception epilogue
                let exception_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.push_call_exc_patch(exception_patch);
                let resolved = self.buf.pos();
                let rel = resolved as i32 - (resolved_patch as i32 + 4);
                Self::patch_or_bail(&mut self.buf, resolved_patch, rel);
                self.patch_ldc_slot_hit(slot_hit);
                self.store_rax(slot);
            }
            // ── cov-05: checkcast ──────────────────────────────────────────
            //
            // Same ABI as the single-pass backend's 0xc0 arm: `jit_checkcast
            // (vm_ptr, obj_ptr, name_ptr, name_len) -> obj_ptr | 0 | i64::MIN`
            // in RAX. Unlike `instanceof`, a definitive refusal stashes a
            // `ClassCastException` and returns the deopt/exception sentinel —
            // `emit_call_return_check` is the SAME sentinel-drain-through-the-
            // shared-epilogue helper `Op::Call` uses for a callee's exception,
            // so this is not new machinery, just a new caller of it. `Ref` is
            // never legitimately `i64::MIN` (no plausible heap pointer is),
            // so it takes that function's simple `CMP ; JE` shape.
            Op::CheckCast { name_ptr, name_len } => {
                let (name_ptr, name_len) = (*name_ptr, *name_len);
                let obj = node.inputs[2];
                // Round rx27 (`CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS`): this
                // target's lambda-proxy cell, planted by `ir_tier_attempt`.
                let cast_proxy_cell = if name_ptr != 0 && ir_checkcast_proxy_ways_enabled() {
                    self.graph
                        .checkcast_proxy_cells
                        .iter()
                        .find(|&&(n, _)| n == name_ptr)
                        .map(|&(_, cell)| cell)
                } else {
                    None
                };
                let proxy_two_hit_ways = ir_proxy_two_hit_ways_enabled();
                // JIT round 23 wave 8 (lane W8-L7, TYPECHECK21-1 part C;
                // `CRATONVM_JIT_IR_CHECKCAST_VALUE_FORWARD`): every reader of
                // this cast reads `obj` (`ir_optimize::checkcast_forward`), so
                // the inline hit paths store no result. The slow path still
                // writes the helper's answer (its return check needs the
                // slot); the slot is un-defined after the arm so no later map
                // publishes a word the hit path never wrote.
                let result_forwarded = self.graph.checkcast_value_forwarded.contains(&id);
                let sp_live_hi = self.spill_high_water;
                // Allocated BEFORE the fast path so that path can store its
                // result, and AFTER `sp_live_hi` is read so the safepoint map
                // below still describes exactly the slots it described when
                // this arm had only one path.
                let slot = self.alloc_slot(id);

                // ---- inline class-id fast path ----
                //
                // The twin of the single-pass backend's `0xc0` arm, and it has
                // to exist HERE as well: this is the door a HOT method reaches.
                // The single-pass fast path on its own measured exactly zero
                // engagement on a four-million-cast probe — `checkcast=
                // 3,203,236` membership walks with it on against `3,203,316`
                // with it off — because every body that mattered had tiered up
                // into the optimizing pipeline, whose `checkcast` lowering is
                // this one. Same lesson the thin native binds recorded: a bind
                // at one compile door is not a bind.
                //
                // The claim is exactly one fact — an object whose class id
                // EQUALS the target's is assignable to it — so this can only
                // turn a slow YES into a fast YES. Subclasses, interfaces,
                // array covariance, lambda proxies and synthetics all have
                // DIFFERENT ids and fall through to the helper unchanged, as
                // does a garbled header, which simply misses. A null is a
                // second fast YES: it passes every `checkcast` and is its own
                // result, so it never pays the helper call and its spill.
                let target_class_id = if name_ptr == 0 {
                    None
                } else {
                    // `Op::CheckCast` carries the interned name as a `usize`
                    // (the node is `Copy`); the intern table is keyed on that
                    // same address, so this is the identical row the helper
                    // call below reaches by pointer.
                    crate::typecheck_target_for_site(name_ptr as *const u8)
                }
                .filter(|&cid| cid != 0 && crate::x64::checkcast_inline_enabled());
                // The 1-D primitive-array variant — see the twin in the
                // single-pass arm. It needs no class id, which is the point: a
                // primitive array's header class id is 0.
                // SAFETY: an `intern_typecheck_target` pair, leaked for the
                // life of the process.
                let site_name = if name_ptr != 0 && crate::x64::checkcast_inline_enabled() {
                    // SAFETY: this interned target pair is leaked for the process lifetime.
                    unsafe { crate::typecheck_site_name(name_ptr as *const u8, name_len) }
                } else {
                    None
                };
                let prim_array_tag = site_name
                    .and_then(cratonvm_types::primitive_array_kind_tags_byte)
                    .filter(|_| crate::x64::checkcast_inline_enabled());
                let mut hit_patch: Option<usize> = None;
                // JIT round 23 wave 9 (lane W9-L7,
                // `CRATONVM_JIT_IR_CHECKCAST_HIT_TO_JOIN`): for a forwarded
                // cast (no reader of its result, no hit-path store) the exact
                // hit and the null branch go straight to the arm's join instead
                // of hopping through `hit_at`'s `JMP join`: one taken branch per
                // hit, not two. Patched to the join at the bottom of the arm.
                let hit_to_join = result_forwarded && ir_checkcast_hit_to_join_enabled();
                let mut to_join: Vec<usize> = Vec::new();
                if let Some(tag) = prim_array_tag {
                    crate::CHECKCAST_INLINE_SITES_PRIM_ARRAY
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.gp_load_value(RAX, obj);
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    let null_slow = self.emit_jcc_rel32(0x84); // JZ → helper
                                                               // CMP BYTE [RAX+KIND_TAGS_BYTE_OFFSET], tag (80 /7 ib).
                    self.buf.emit(&[
                        0x80,
                        0x78,
                        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                        tag,
                    ]);
                    let miss_slow = self.emit_jcc_rel32(0x85); // JNE → helper
                    // A null passes every `checkcast` (JVMS 6.5) and its
                    // result is the null already in RAX: it joins the hit.
                    let hit_at = self.buf.pos();
                    if !result_forwarded {
                        self.store_rax(slot);
                    }
                    hit_patch = Some(self.emit_jmp_rel32());
                    self.patch_rel32_to_target(null_slow, hit_at);
                    self.patch_rel32_to_here(miss_slow);
                } else if let Some(cid) = target_class_id {
                    if self.try_elide_proven_checkcast(id, obj, slot, result_forwarded, cid) {
                        return;
                    }
                    let (final_fast, outlined) = self.try_emit_exact_final_checkcast_or_outline(
                        id,
                        obj,
                        slot,
                        name_ptr,
                        name_len,
                        cid,
                        cast_proxy_cell,
                        result_forwarded,
                        hit_to_join,
                        sp_live_hi,
                        &mut hit_patch,
                        &mut to_join,
                    );
                    if outlined {
                        return;
                    }
                    if !final_fast {
                    crate::CHECKCAST_INLINE_SITES_IR
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // Asked before the first byte (lane tcfuse moved it up
                    // from after the kind screen; no code depends on it).
                    let display = self.typecheck_display_for(cid, "checkcast");
                    let mut first_hits: Vec<usize> = Vec::new();
                    let mut first_no_miss: Vec<usize> = Vec::new();
                    self.gp_load_value(RAX, obj);
                    // JIT round 21, lane tcfuse (TCPERF21-3): the row arm's
                    // hit from the shared class word, ahead of the screens.
                    let word_row_first = match display {
                        Some(d) => self.emit_typecheck_row_first_arm_from_word(
                            id,
                            obj,
                            d,
                            cid,
                            false,
                            "checkcast",
                            site_name,
                            &mut first_hits,
                            &mut first_no_miss,
                        ),
                        None => false,
                    };
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    let null_slow = self.emit_jcc_rel32(0x84); // JZ → helper
                    // JIT r21 wave 1 (lane typecheck, TYPECHECK21-3,
                    // `CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS`): a reference-array
                    // receiver of a `[LT;` cast whose component is proven a
                    // subtype of `T` joins the hit below; a non-proof calls the
                    // helper, which owns the ClassCastException.
                    let mut array_hits: Vec<usize> = Vec::new();
                    let mut array_slow: Vec<usize> = Vec::new();
                    // SAFETY: an `intern_typecheck_target` pair, leaked for the
                    // life of the process.
                    let cast_site_name =
                        // SAFETY: this interned target pair is leaked for the process lifetime.
                    unsafe { crate::typecheck_site_name(name_ptr as *const u8, name_len) };
                    if let Some(plan) = self.typecheck_array_plan_for(cast_site_name, cid, "checkcast") {
                        self.emit_typecheck_array_arm(plan, "checkcast", &mut array_hits, &mut array_slow, None);
                    }
                                                               // ARRAY RECEIVERS MUST NOT REACH THE COMPARE — see the
                                                               // twin of this guard in the single-pass `0xc0` arm, and
                                                               // `regression-suite` `RJitArrayTypecheck`, which caught
                                                               // this fix's first version. An array's header holds its
                                                               // COMPONENT class id (or 0, for a primitive array), never
                                                               // its own, so `checkcast java/lang/String` on a `String[]`
                                                               // would match the baked target and accept a cast that must
                                                               // throw. False since the 8-byte header: a HASHED compact
                                                               // instance keeps identity-hash bits in that byte
                                                               // (`MARK_SHORT_HASH_HI_MASK`). Only its `kind` bits are
                                                               // zero, which is why the guard below is
                                                               // `TEST ..., KIND_TAG_BYTE_MASK`. A future edit that
                                                               // believed this comment and wrote `CMP BYTE [..], 0`
                                                               // would send every hashed receiver to the helper (and, in
                                                               // an `instanceof` fast path answering "not an instance" on
                                                               // a miss, give a wrong answer).
                                                               // TEST BYTE [RAX+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK (F6 /0 ib: kind bits only)
                    // The guard: TEST BYTE [RAX+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK.
                    self.buf.emit(&[
                        0xF6,
                        0x40,
                        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                        cratonvm_types::KIND_TAG_BYTE_MASK,
                    ]);
                    let kind_slow = self.emit_jcc_rel32(0x85); // JNE → helper
                    // JIT round 21, lane tcperf (`CRATONVM_JIT_TYPECHECK_ROW_FIRST`
                    // / `_IFACE`): the status-first row arm's hit before
                    // anything else; a cast answers no miss inline. Not twice
                    // after the shared-class-word arm (lane tcfuse).
                    let row_first = word_row_first
                        || match display {
                            Some(d) => self.emit_typecheck_row_first_arm(
                                d,
                                cid,
                                false,
                                "checkcast",
                                site_name,
                                &mut first_hits,
                                &mut first_no_miss,
                            ),
                            None => false,
                        };
                    debug_assert!(first_no_miss.is_empty());
                    // Round rx27 (`CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS`): a
                    // lambda-proxy receiver (a negative i32 id) passes when a
                    // hit way holds its id, and otherwise goes straight to the
                    // helper (no compare or display below answers that
                    // range). A hierarchy id skips the arm.
                    // CMP DWORD [RAX], 0 ; JGE past ; per hit way:
                    // MOV R11, cell ; MOV R11D, [R11+d] ; CMP [RAX], R11D ;
                    // JE hit ; then JMP helper.
                    let mut proxy_hits: Vec<usize> = Vec::new();
                    let mut proxy_slow: Option<usize> = None;
                    if let Some(cell) = cast_proxy_cell {
                        self.buf.emit(&[0x83, 0x78, 0x00, 0x00]);
                        let hierarchy = self.emit_jcc_rel32(0x8D);
                        let ways: &[u8] = if proxy_two_hit_ways {
                            &[PROXY_WAY_HIT_DISP, PROXY_WAY_HIT2_DISP]
                        } else {
                            &[PROXY_WAY_HIT_DISP]
                        };
                        for &disp in ways {
                            self.emit_mov_reg_imm64(R11, cell as u64);
                            self.buf.emit(&[0x45, 0x8B, 0x5B, disp]);
                            self.buf.emit(&[0x44, 0x39, 0x18]);
                            proxy_hits.push(self.emit_jcc_rel32(0x84));
                        }
                        proxy_slow = Some(self.emit_jmp_rel32());
                        self.patch_rel32_to_here(hierarchy);
                        if crate::x64::dbg_jitc_enabled() {
                            eprintln!(
                                "[cratonvm-jitc] ir checkcast proxy ways: target={cid} cell={cell:#x} hit_ways={}",
                                ways.len()
                            );
                        }
                    }
                                                               // CMP DWORD [RAX+0], cid. `ObjectHeader.class_id` is a
                                                               // plain `u32` at offset 0 by contract — its own doc comment
                                                               // says "JIT-emitted type guards are `CMP DWORD [recv+0],
                                                               // imm`" and `class_id_remains_at_offset_zero` enforces it —
                                                               // and this is byte-for-byte the encoding the guarded-virtual
                                                               // inline arm already emits (81 /7 id, ModRM 0x78 = mod01
                                                               // disp8 /7 rm=RAX).
                    self.buf.emit(&[0x81, 0x78, 0x00]);
                    self.buf.emit(&cid.to_le_bytes());
                    // JIT r20 wave 11 (lane pAlloc, W14L4-3): a miss asks the
                    // supers display before the helper; a display hit joins the
                    // exact hit below, anything else declines to the call.
                    let mut display_slow: Vec<usize> = Vec::new();
                    let miss_slow = match display {
                        Some(display) => {
                            let mut hits = vec![self.emit_jcc_rel32(0x84)]; // JE → hit
                            hits.append(&mut first_hits);
                            // JIT round 21, lane dispatch
                            // (`CRATONVM_JIT_TYPECHECK_ROWS`): the receiver's
                            // type-check row answers a hit first; anything
                            // else falls through to the display arm. A cast
                            // answers no miss inline (the helper throws).
                            // Not twice: skipped after the row-first arm.
                            let mut no_miss: Vec<usize> = Vec::new();
                            if !row_first {
                                self.emit_typecheck_row_arm(
                                    display,
                                    cid,
                                    false,
                                    "checkcast",
                                    site_name,
                                    &mut hits,
                                    &mut no_miss,
                                );
                            }
                            debug_assert!(no_miss.is_empty());
                            self.emit_typecheck_display_hit(display, cid, &mut display_slow);
                            if hit_to_join {
                                to_join.append(&mut hits);
                            } else {
                                for p in hits {
                                    self.patch_rel32_to_here(p);
                                }
                            }
                            None
                        }
                        None => Some(self.emit_jcc_rel32(0x85)), // JNE → helper
                    };
                                                               // Hit: the receiver IS the target class, so the cast
                                                               // succeeds and its result is the reference already in RAX —
                                                               // the same value the helper would have returned. No
                                                               // safepoint map, no frame-record republish and no
                                                               // sentinel drain, because this path makes no call.
                                                               // A null joins it: it passes every `checkcast`
                                                               // (JVMS 6.5) and RAX already holds the result.
                    let hit_at = self.buf.pos();
                    if !result_forwarded {
                        self.store_rax(slot);
                    }
                    hit_patch = Some(self.emit_jmp_rel32());
                    if hit_to_join {
                        to_join.push(null_slow);
                    } else {
                        self.patch_rel32_to_target(null_slow, hit_at);
                    }
                    for p in array_hits {
                        self.patch_rel32_to_target(p, hit_at);
                    }
                    for p in proxy_hits {
                        self.patch_rel32_to_target(p, hit_at);
                    }
                    self.patch_rel32_to_here(kind_slow);
                    if let Some(p) = proxy_slow {
                        self.patch_rel32_to_here(p);
                    }
                    for p in array_slow {
                        self.patch_rel32_to_here(p);
                    }
                    if let Some(miss_slow) = miss_slow {
                        self.patch_rel32_to_here(miss_slow);
                    }
                    for p in display_slow {
                        self.patch_rel32_to_here(p);
                    }
                    }
                } else {
                    // No fast path for this target: a null still never needs
                    // the helper (JVMS 6.5), and a `(String) null` argument is
                    // exactly what a spliced `assertEquals(a, b)` forwards on
                    // every call.
                    self.gp_load_value(RAX, obj);
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    let not_null = self.emit_jcc_rel32(0x85); // JNZ → helper
                    if !result_forwarded {
                        self.store_rax(slot);
                    }
                    hit_patch = Some(self.emit_jmp_rel32());
                    self.patch_rel32_to_here(not_null);
                }

                // Round 9: `alloc_slot` above marked this node's OWN result
                // slot `defined_nodes[id]`, and `emit_safepoint_map` publishes
                // every defined `Ref` slot as a root — `live_hi` does not filter
                // it. On the slow path the slot is not written until
                // `jit_checkcast` returns, so the map would hand the collector
                // whatever an earlier value sharing this colour left there (or
                // uninitialised stack). The `NewArray` arm avoids this by
                // mapping before `alloc_slot`; this arm needs the slot for its
                // fast-path store, so it hides the mark for the map instead.
                let was_defined = std::mem::replace(&mut self.defined_nodes[id as usize], false);
                let mut map = self.emit_safepoint_map(sp_live_hi);
                self.defined_nodes[id as usize] = was_defined;
                self.load_context(CALL_ARG_REGS[0]); // vm_ptr
                self.gp_load_value(CALL_ARG_REGS[1], obj); // obj_ptr
                self.emit_mov_reg_imm64(CALL_ARG_REGS[2], name_ptr as u64); // name_ptr
                self.emit_mov_reg_imm64(CALL_ARG_REGS[3], name_len as u64); // name_len
                self.emit_mov_reg_imm64(RAX, self.checkcast as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
                self.note_inline_frame_return_site();
                // `emit_call_return_check`, with the type check's FP reloads
                // (JIT round 21, lane tcmiss, TCPERF21-1) after the frame
                // republish and shadow reload and before the sentinel exit.
                self.emit_post_call_frame_record();
                self.emit_shadow_reload();
                self.emit_poll_fp_reloads(TYPECHECK_FP_RELOAD_KEY | id as usize);
                self.emit_call_return_sentinel_tail(IrType::Ref);
                // Round rx27 (`CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS`): a cast
                // the helper passed for a plain lambda-proxy receiver writes
                // its id into the hit way(s). The helper's returned reference
                // is read (non-null on success; the fail-soft `0` writes
                // nothing); RAX is kept.
                // TEST RAX, RAX ; JZ keep ; TEST BYTE [RAX+KIND_TAGS], MASK ;
                // JNE keep ; MOV R11D, [RAX] ; TEST R11D, R11D ; JNS keep ;
                // MOV RCX, cell ; the fill ([`proxy_hit_way_fill_bytes`]).
                if let (Some(cell), Some(_)) = (cast_proxy_cell, target_class_id) {
                    self.buf.emit(&[0x48, 0x85, 0xC0]);
                    let null = self.emit_jcc_rel32(0x84);
                    self.buf.emit(&[
                        0xF6,
                        0x40,
                        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                        cratonvm_types::KIND_TAG_BYTE_MASK,
                    ]);
                    let array = self.emit_jcc_rel32(0x85);
                    self.buf.emit(&[0x44, 0x8B, 0x18]); // MOV R11D, [RAX]
                    self.buf.emit(&[0x45, 0x85, 0xDB]); // TEST R11D, R11D
                    let hierarchy = self.emit_jcc_rel32(0x89); // JNS -> keep
                    self.note_rcx_written();
                    self.emit_mov_reg_imm64(RCX, cell as u64);
                    self.buf.emit(&proxy_hit_way_fill_bytes(true, proxy_two_hit_ways));
                    for p in [null, array, hierarchy] {
                        self.patch_rel32_to_here(p);
                    }
                }
                self.store_rax(slot);
                if let Some(p) = hit_patch {
                    self.patch_rel32_to_here(p);
                }
                for p in to_join {
                    self.patch_rel32_to_here(p);
                }
                // JIT r23 wave 8 (W8-L7): a forwarded cast's slot holds the
                // helper's answer on the slow path only; nothing reads it,
                // and no later safepoint map may publish it.
                if result_forwarded {
                    if let Some(cell) = self.defined_nodes.get_mut(id as usize) {
                        *cell = false;
                    }
                }
            }
            // ── cov-05 increment 1: instanceof ────────────────────────────
            //
            // Same ABI as the single-pass backend's 0xc1 arm
            // (`x64/bytecode_walk.rs`): `jit_instanceof(vm_ptr, obj_ptr,
            // name_ptr, name_len) -> 0/1` in RAX. Reusing that helper is the
            // point — this node's whole job is to answer the subtype question
            // the single-pass backend already answers correctly (strict array
            // rule, loader-dup fallback, every recorded typecheck defect fix
            // included), not to re-derive one.
            //
            // Unlike `Op::ConstClass` this never meets a failure sentinel, so
            // there is no post-call TEST/JNZ/exception-epilogue dance here —
            // just the safepoint map (the helper can still allocate a Class
            // mirror on first touch) and the result in RAX. `jit_instanceof`
            // raises (i11-L2: `IllegalAccessError` / a resolution error, as
            // `i64::MIN`) only at a site interned under its HOLDER, i.e. one
            // its compile door could not bind, and this tier admits an
            // `instanceof` only with a compile-time-resolved target
            // (`instanceof_info`; an unbound site plants an
            // `UnresolvedTypeCheck` trap in `ir.rs`).
            Op::InstanceOf {
                name_ptr,
                name_len,
                seen_cell,
                ..
            } => {
                if self.fused_cmp.get(id as usize).copied().unwrap_or(false) {
                    return;
                }
                let (name_ptr, name_len, seen_cell) = (*name_ptr, *name_len, *seen_cell);
                // JIT r25 (`CRATONVM_JIT_IR_INSTANCEOF_PROXY_WAYS`): a tagged
                // cell is the lambda-proxy ways, not MHIFACE19-1's seen-class
                // ways, which then stay off for this site.
                let (proxy_cell, seen_cell) = if seen_cell & INSTANCEOF_PROXY_CELL_TAG != 0 {
                    (seen_cell & !INSTANCEOF_PROXY_CELL_TAG, 0)
                } else {
                    (0, seen_cell)
                };
                let obj = node.inputs[2];
                let sp_live_hi = self.spill_high_water;

                // ---- inline fast path (round 9 wave 9, irl9) ----
                //
                // The twin of the single-pass `0xc1` arm's guards
                // (`x64/op_object.rs`), which this tier never had: every
                // compiled `instanceof` here was a `jit_instanceof` CALL, hit
                // or miss. `perf-instanceof-final-user-class-fast-path-misses-
                // in-a-mixed-main` measured the cost: the same `instOf` loop
                // took 30 ms through the single-pass body and ~550 ms once
                // the C2 body superseded it (`CRATONVM_C2_SUPERSEDE=0` brings
                // it back to 37 ms). Answers without a call:
                //
                // * null -> 0 (JVMS 6.5);
                // * a plain object (KIND_TAGS == 0) whose header class id IS
                //   the resolved target -> 1;
                // * a 1-D primitive array whose KIND_TAGS byte is the
                //   target's -> 1, and a plain object against such a target
                //   -> 0;
                // * a definite miss for a final target (a final boot class,
                //   or a site its compile door interned as a proven-final
                //   class) -> 0, behind the SAME two run-time screens as the
                //   single-pass latch: a receiver id >= 0x8000_0000 (lambda
                //   proxy / autobox wrapper) and a raised duplicate-class-name
                //   latch both still ask the helper.
                //
                // Everything else -- arrays against a class target, a miss
                // against a non-final target -- falls through to the helper
                // call below, unchanged. The stubs are placed BEFORE the
                // safepoint map, so a fast answer skips the map's shadow push
                // and the post-call reload together (the balance
                // `Op::ConstString`'s slot probe relies on too), and they
                // join at the single `store_rax` at the end of the arm.
                // Gated by `CRATONVM_JIT_CHECKCAST_INLINE` like the IR
                // `checkcast` fast path; the final miss additionally by
                // `CRATONVM_JIT_INSTANCEOF_FINAL_MISS` (default on).
                let fast_enabled = name_ptr != 0 && crate::x64::checkcast_inline_enabled();
                let site_name = if fast_enabled {
                    // SAFETY: `name_ptr`/`name_len` are an `intern_typecheck_target`
                    // pair, leaked for the life of the process.
                    // SAFETY: this interned target pair is leaked for the process lifetime.
                    unsafe { crate::typecheck_site_name(name_ptr as *const u8, name_len) }
                } else {
                    None
                };
                let prim_array_tag =
                    site_name.and_then(cratonvm_types::primitive_array_kind_tags_byte);
                let target_class_id = if fast_enabled && prim_array_tag.is_none() {
                    crate::typecheck_target_for_site(name_ptr as *const u8).filter(|&cid| cid != 0)
                } else {
                    None
                };
                let final_miss_shape = target_class_id.is_some_and(|cid| {
                    ir_instanceof_final_miss_enabled()
                        && (site_name.is_some_and(ir_instanceof_miss_is_final_boot_class)
                            || crate::typecheck_target_is_final_class(name_ptr as *const u8)
                            || self.cha_leaf_classes.contains(&cid))
                });
                // JIT r21 wave 1 (lane typecheck,
                // `CRATONVM_JIT_INSTANCEOF_FINAL_MISS_VERDICT`, default ON):
                // the definite miss also needs the receiver display's
                // hierarchy-only verdict, so an instance-admitted receiver (an
                // `Unmodifiable*` stamp displaying as a FINAL JDK class such as
                // `ImmutableCollections$ListN`) asks the helper. Without
                // displays the miss is not answered inline at all. `=0`
                // restores the id-range and latch screens alone.
                let final_miss_verdict_cell = if final_miss_shape && instanceof_final_miss_verdict_enabled() {
                    Some(self.class_assignable_display_cell())
                } else {
                    None
                };
                let final_miss = final_miss_shape && !matches!(final_miss_verdict_cell, Some(None));
                let final_miss_verdict_cell = final_miss_verdict_cell.flatten();
                // ---- the site census (r10 wave 9) ----
                //
                // The twin of the single-pass arm's, in the same `if`/`else if`
                // order and under the same `checkcast_inline_enabled()` gate, so
                // the two halves of the `[cratonvm] JIT instanceof inline sites:`
                // line read term by term. Closes
                // `docs/feature-designs/jit-ir-optimizations.md`,
                // whose point was that THIS is the door a hot method reaches —
                // the same argument `CHECKCAST_INLINE_SITES_IR`'s doc makes, and
                // the reason the `irl9` change above exists at all (30 ms through
                // the single-pass body, ~550 ms once the C2 body superseded it).
                //
                // FOUR buckets against the single-pass arm's five, deliberately:
                // there is no `refused-untrusted-operand` here and there must not
                // be one. That bucket is fed by `operand_is_trusted_oop`, a test
                // over the single-pass backend's `stack_oop_marks`; this tier's
                // operand is an SSA value and its reference-ness is a property of
                // the IR, so the population is empty by construction. See
                // [`crate::instanceof_ir_inline_sites`].
                //
                // The `else` arm here is NOT the single-pass `no_target_id`
                // bucket's twin in population, only in name. An `instanceof`
                // whose target is not loaded at compile time never reaches this
                // lowering at all — `lib.rs`'s `instanceof_info` gate omits the
                // site and `ir.rs`'s `0xc1` arm plants an `UnresolvedTypeCheck`
                // trap — so what lands here is the OTHER sub-cause of the shared
                // `.filter(|&id| id != 0)`: a target whose resolved class id is
                // `0`, which `ClassStore::next_id` gives to the first class the
                // VM loads. Live, not a tripwire.
                //
                // Counted at EMISSION, not in the predicate (review #80). The
                // `if` chain is over exactly the conditions the emitter below
                // branches on — `prim_array_tag.is_some() ||
                // target_class_id.is_some()` is its own guard, and
                // `target_class_id` is already filtered on
                // `prim_array_tag.is_none()` — so there is no path on which a
                // counter moves and the corresponding stub is not written.
                // `final_miss` is bumped apart from the chain because it is a
                // SUBSET of the class-id bucket (the same site, extended with the
                // definite-miss stub); making it exclusive would change what
                // `class-id` means, and it implies `target_class_id.is_some()` so
                // the `else if` arm is always the one taken.
                if crate::x64::checkcast_inline_enabled() {
                    if prim_array_tag.is_some() {
                        crate::note_instanceof_ir_inline_prim_array_site();
                    } else if target_class_id.is_some() {
                        crate::note_instanceof_ir_inline_class_id_site();
                    } else {
                        crate::note_instanceof_ir_refused_no_target_id();
                    }
                    if final_miss {
                        crate::note_instanceof_ir_inline_final_miss_site();
                    }
                }
                let mut answered: Vec<usize> = Vec::new();
                if prim_array_tag.is_some() || target_class_id.is_some() {
                    let mut zero_patches: Vec<usize> = Vec::new();
                    let mut one_patches: Vec<usize> = Vec::new();
                    let mut slow_patches: Vec<usize> = Vec::new();
                    // The display (and its miss screen) is asked once per
                    // site, before any byte: the round-21 row arms and the
                    // display arm all read the two answers.
                    let class_display = match target_class_id {
                        Some(cid) if !final_miss => self.typecheck_display_for(cid, "instanceof"),
                        _ => None,
                    };
                    let class_display_miss_ok = match (target_class_id, class_display) {
                        (Some(cid), Some(_)) => self.typecheck_display_miss_for(cid, site_name),
                        _ => false,
                    };
                    self.gp_load_value(RAX, obj);
                    // JIT round 21, lane tcfuse (TCPERF21-3): from the shared
                    // class word, ahead of the null and kind screens.
                    let word_row_first = match (target_class_id, class_display) {
                        (Some(cid), Some(d)) => self.emit_typecheck_row_first_arm_from_word(
                            id,
                            obj,
                            d,
                            cid,
                            class_display_miss_ok,
                            "instanceof",
                            site_name,
                            &mut one_patches,
                            &mut zero_patches,
                        ),
                        _ => false,
                    };
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    zero_patches.push(self.emit_jcc_rel32(0x84)); // JZ -> `0`
                    if let Some(tag) = prim_array_tag {
                        // CMP BYTE [RAX+KIND_TAGS_BYTE_OFFSET], tag ; JE -> `1`.
                        self.buf.emit(&[
                            0x80,
                            0x78,
                            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                            tag,
                        ]);
                        one_patches.push(self.emit_jcc_rel32(0x84));
                        // A PLAIN object is never a primitive array:
                        // TEST BYTE [RAX+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK (F6 /0 ib: kind bits only) ; JE -> `0`.
                        self.buf.emit(&[
                            0xF6,
                            0x40,
                            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                            cratonvm_types::KIND_TAG_BYTE_MASK,
                        ]);
                        zero_patches.push(self.emit_jcc_rel32(0x84));
                        slow_patches.push(self.emit_jmp_rel32()); // any other array
                    } else if let Some(cid) = target_class_id {
                        // JIT r21 wave 1 (lane typecheck, TYPECHECK21-3,
                        // `CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS`): an array
                        // receiver of a `[LT;` target is answered `1` from its
                        // component ([`typecheck_array_hit_bytes`]) before the
                        // plain-object arms; a non-proof asks the helper.
                        if let Some(plan) = self.typecheck_array_plan_for(site_name, cid, "instanceof") {
                            self.emit_typecheck_array_arm(
                                plan,
                                "instanceof",
                                &mut one_patches,
                                &mut slow_patches,
                                Some(&mut zero_patches),
                            );
                        }

                        // Arrays never reach the id compare: an array header
                        // carries its COMPONENT's id (BUG-JIT-ARRAY-INSTANCEOF).
                        // TEST BYTE [RAX+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK (F6 /0 ib: kind bits only) ; JNE -> helper.
                        self.buf.emit(&[
                            0xF6,
                            0x40,
                            cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                            cratonvm_types::KIND_TAG_BYTE_MASK,
                        ]);
                        slow_patches.push(self.emit_jcc_rel32(0x85));
                        // The display and its miss screen, asked before the
                        // first byte (above).
                        let display = class_display;
                        let display_miss_ok = class_display_miss_ok;
                        // JIT round 21, lane tcperf (`CRATONVM_JIT_TYPECHECK_ROW_FIRST`
                        // / `_IFACE`): the status-first row arm before anything
                        // else -- HotSpot's one-compare shape for a class at
                        // depth 1..7 or a placed interface; the exact compare,
                        // the seen ways and the display arm below serve what it
                        // does not answer. Not twice: a site that took it from
                        // the shared class word (lane tcfuse) skips it.
                        let row_first = word_row_first
                            || match display {
                                Some(d) => self.emit_typecheck_row_first_arm(
                                    d,
                                    cid,
                                    display_miss_ok,
                                    "instanceof",
                                    site_name,
                                    &mut one_patches,
                                    &mut zero_patches,
                                ),
                                None => false,
                            };
                        // CMP DWORD [RAX+0], cid ; JE -> `1`. `class_id` is the
                        // `u32` at offset 0, as in the `Op::CheckCast` arm.
                        self.buf.emit(&[0x81, 0x78, 0x00]);
                        self.buf.emit(&cid.to_le_bytes());
                        one_patches.push(self.emit_jcc_rel32(0x84));
                        // JIT round 20 (MHIFACE19-1): the site's two seen-class
                        // ways -- the first two plain object classes the helper
                        // answered `1` for here, or the target's own id until
                        // then (which the compare above already answered).
                        // A class id never changes for an object's lifetime,
                        // and a class's supertypes never change, so a hit is
                        // the helper's answer.
                        // MOV R11, cell ; MOV R11D, [R11+4k] ; CMP [RAX], R11D ;
                        // JE -> `1`, for way 0 then way 1.
                        if seen_cell != 0 && !final_miss {
                            for way_disp in [0u8, 4] {
                                self.emit_mov_reg_imm64(R11, seen_cell as u64);
                                self.buf.emit(&[0x45, 0x8B, 0x5B, way_disp]);
                                self.buf.emit(&[0x44, 0x39, 0x18]);
                                one_patches.push(self.emit_jcc_rel32(0x84));
                            }
                        }
                        // JIT r25 (`CRATONVM_JIT_IR_INSTANCEOF_PROXY_WAYS`):
                        // a lambda-proxy receiver (id >= 0x8000_0000, a
                        // negative i32) is answered from the proxy ways --
                        // way 0 a proven `1`, way 1 a proven `0` behind the
                        // duplicate-name latch -- and any other proxy goes
                        // straight to the helper, which the arms below would
                        // send it to anyway (no display or final miss covers
                        // that range). Both ways start at 0, which no proxy
                        // id equals. A hierarchy id skips the arm.
                        // CMP DWORD [RAX], 0 ; JGE past ;
                        // MOV R11, cell ; MOV R11D, [R11] ; CMP [RAX], R11D ; JE `1` ;
                        // MOV R11, cell ; MOV R11D, [R11+4] ; CMP [RAX], R11D ; JNE helper ;
                        // MOV R11, &latch ; CMP BYTE [R11], 0 ; JNE helper ; JMP `0`.
                        if proxy_cell != 0 && !final_miss {
                            self.buf.emit(&[0x83, 0x78, 0x00, 0x00]);
                            let hierarchy = self.emit_jcc_rel32(0x8D);
                            self.emit_mov_reg_imm64(R11, proxy_cell as u64);
                            self.buf.emit(&[0x45, 0x8B, 0x5B, PROXY_WAY_HIT_DISP]);
                            self.buf.emit(&[0x44, 0x39, 0x18]);
                            one_patches.push(self.emit_jcc_rel32(0x84));
                            // Round rx27 (`CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS`):
                            // the second hit way, the same three instructions.
                            if ir_proxy_two_hit_ways_enabled() {
                                self.emit_mov_reg_imm64(R11, proxy_cell as u64);
                                self.buf.emit(&[0x45, 0x8B, 0x5B, PROXY_WAY_HIT2_DISP]);
                                self.buf.emit(&[0x44, 0x39, 0x18]);
                                one_patches.push(self.emit_jcc_rel32(0x84));
                            }
                            self.emit_mov_reg_imm64(R11, proxy_cell as u64);
                            self.buf.emit(&[0x45, 0x8B, 0x5B, PROXY_WAY_MISS_DISP]);
                            self.buf.emit(&[0x44, 0x39, 0x18]);
                            slow_patches.push(self.emit_jcc_rel32(0x85));
                            let latch = cratonvm_types::duplicate_class_name_seen_addr();
                            self.emit_mov_reg_imm64(R11, latch as u64);
                            self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]);
                            slow_patches.push(self.emit_jcc_rel32(0x85));
                            zero_patches.push(self.emit_jmp_rel32());
                            self.patch_rel32_to_here(hierarchy);
                            if crate::x64::dbg_jitc_enabled() {
                                eprintln!(
                                    "[cratonvm-jitc] ir instanceof proxy ways: target={cid} cell={proxy_cell:#x}"
                                );
                            }
                        }

                        // JIT round 21, lane dispatch (`CRATONVM_JIT_TYPECHECK_ROWS`,
                        // opt-in since the w1g A/B): the receiver's type-check row,
                        // AFTER the seen ways (which already answer their two
                        // classes in one compare each), in front of the display
                        // arm: a hit in one compare, a hierarchy-only miss in a
                        // few more; any other outcome falls through to the
                        // display arm below, unchanged. Interface targets only
                        // unless `CRATONVM_JIT_TYPECHECK_ROWS_ALL`. Not twice:
                        // a site that took the row-first arm skips it.
                        if let (Some(display), false) = (display, row_first) {
                            self.emit_typecheck_row_arm(
                                display,
                                cid,
                                display_miss_ok,
                                "instanceof",
                                site_name,
                                &mut one_patches,
                                &mut zero_patches,
                            );
                        }
                        if final_miss {
                            self.emit_instanceof_final_miss_arm(
                                cid,
                                site_name,
                                final_miss_verdict_cell,
                                &mut slow_patches,
                                &mut zero_patches,
                            );
                        } else if let Some(display) = display {
                            // JIT r20 wave 11 (lane pAlloc, W14L4-3): a subclass
                            // or implementor the seen ways missed answers `1`
                            // from the supers display; every other outcome
                            // (no display yet, not listed) asks the helper,
                            // which alone may answer a miss.
                            //
                            // JIT r20 wave 17 (lane pAlloc2,
                            // `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS`): a miss of
                            // a hierarchy-only receiver answers `0` too.
                            if display_miss_ok {
                                // JIT r23 w2c close (`CRATONVM_JIT_TYPECHECK_MISS_MEMO`):
                                // the memo first (behind the latch the
                                // display answer's own miss reads), and the
                                // display's proven miss fills it.
                                let memo = crate::typecheck_miss_memo_enabled()
                                    .then(|| crate::typecheck_miss_memo_cell(&self.direct_helpers, cid))
                                    .flatten();
                                let mut miss_patches: Vec<usize> = Vec::new();
                                let mut hit_patches: Vec<usize> = Vec::new();
                                if let Some(m) = memo {
                                    // The hit ways first: a display hit needs no latch.
                                    self.emit_typecheck_miss_memo_probe(m + 8, &mut one_patches);
                                    if !typecheck_site_is_boot_package(site_name) {
                                        let latch = cratonvm_types::duplicate_class_name_seen_addr();
                                        self.emit_mov_reg_imm64(R11, latch as u64);
                                        self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
                                        let raised = self.emit_jcc_rel32(0x85);
                                        self.emit_typecheck_miss_memo_probe(m, &mut zero_patches);
                                        self.patch_rel32_to_here(raised);
                                    } else {
                                        self.emit_typecheck_miss_memo_probe(m, &mut zero_patches);
                                    }
                                }
                                self.emit_typecheck_display_answer(
                                    display,
                                    cid,
                                    site_name,
                                    &mut slow_patches,
                                    if memo.is_some() {
                                        &mut hit_patches
                                    } else {
                                        &mut one_patches
                                    },
                                    if memo.is_some() {
                                        &mut miss_patches
                                    } else {
                                        &mut zero_patches
                                    },
                                );
                                if let Some(m) = memo {
                                    for p in miss_patches {
                                        self.patch_rel32_to_here(p);
                                    }
                                    self.emit_typecheck_miss_memo_fill(m, cid, &mut zero_patches);
                                    zero_patches.push(self.emit_jmp_rel32());
                                    for p in hit_patches {
                                        self.patch_rel32_to_here(p);
                                    }
                                    self.emit_typecheck_miss_memo_fill(m + 8, cid, &mut one_patches);
                                    one_patches.push(self.emit_jmp_rel32());
                                }
                            } else {
                                self.emit_typecheck_display_hit(
                                    display,
                                    cid,
                                    &mut slow_patches,
                                );
                                one_patches.push(self.emit_jmp_rel32());
                            }
                        } else {
                            slow_patches.push(self.emit_jmp_rel32());
                        }
                    }
                    // JIT round 21, lane tcfuse (TCPERF21-2 = DISPATCH21-3,
                    // `CRATONVM_JIT_TYPECHECK_BRANCH_FUSION`): an `if (o
                    // instanceof T)` takes its inline answers straight to the
                    // `If`'s edges (`typecheck_branch_fusion_for` says when
                    // that skips nothing the edges need); the `If` arm
                    // patches them. The helper path below is unchanged.
                    let fused = if one_patches.is_empty() && zero_patches.is_empty() {
                        Err("no inline answer")
                    } else {
                        self.typecheck_branch_fusion_for(id)
                    };
                    if crate::x64::dbg_jitc_enabled()
                        && crate::typecheck_rows::typecheck_branch_fusion_enabled()
                    {
                        eprintln!(
                            "[cratonvm-jitc] ir instanceof branch fusion: n{id} {}",
                            match fused {
                                Ok((cond, _, false)) => format!("FUSED into the If of n{cond}"),
                                Ok((cond, _, true)) => {
                                    format!("FUSED into the If of n{cond} (named only where no deopt reads it)")
                                }
                                Err(why) => format!("declined -- {why}"),
                            }
                        );
                    }
                    if let Ok((cond, one_is_true, unnamed)) = fused {
                        if unnamed {
                            // No location from here on: a frame state that
                            // describes it fails the attempt closed
                            // (`frame_value_for`), which is then lowered
                            // again with every home kept.
                            if let Some(cell) = self.fused_cmp_unnamed.get_mut(id as usize) {
                                if !*cell {
                                    *cell = true;
                                    self.fused_cmp_unnamed_count += 1;
                                }
                            }
                        }
                        self.typecheck_branch_pending =
                            Some((cond, one_patches, zero_patches, one_is_true));
                    } else {
                        for p in one_patches {
                            self.patch_rel32_to_here(p);
                        }
                        // MOV EAX, 1 (zero-extends into RAX).
                        self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
                        answered.push(self.emit_jmp_rel32());
                        for p in zero_patches {
                            self.patch_rel32_to_here(p);
                        }
                        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
                        answered.push(self.emit_jmp_rel32());
                    }
                    for p in slow_patches {
                        self.patch_rel32_to_here(p);
                    }
                }

                let mut map = self.emit_safepoint_map(sp_live_hi);
                let slot = self.alloc_slot(id);
                self.load_context(CALL_ARG_REGS[0]); // vm_ptr
                self.gp_load_value(CALL_ARG_REGS[1], obj); // obj_ptr
                self.emit_mov_reg_imm64(CALL_ARG_REGS[2], name_ptr as u64); // name_ptr
                self.emit_mov_reg_imm64(CALL_ARG_REGS[3], name_len as u64); // name_len
                self.emit_mov_reg_imm64(RAX, self.instanceof_check as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
                                              // The helper can load a class (and so collect or throw) inside
                                              // a spliced callee: record the inline chain at the return
                                              // address (round 11 wave 2, lane lower).
                self.note_inline_frame_return_site();
                                              // Same post-call obligation as `Op::ConstString`: the helper
                                              // crossed the JIT boundary and may have run a collection, so
                                              // this frame's mirror and any relocated published value must
                                              // be restored before anything else reads the frame.
                self.emit_post_call_frame_record();
                self.emit_shadow_reload();
                // JIT round 21, lane tcmiss (TCPERF21-1): the FP values resident
                // in a caller-saved XMM across this check, from their homes.
                self.emit_poll_fp_reloads(TYPECHECK_FP_RELOAD_KEY | id as usize);
                // JIT round 20 (MHIFACE19-1): the first TWO helper `1`s for plain
                // objects whose class id is a hierarchy one (below 0x8000_0000;
                // the others are the helper's non-hierarchy admissions) become
                // the site's seen classes: a way is written only while it still
                // holds the target's own id (way 0 first), like a two-entry
                // inline cache, so a site with more classes pays two compares and
                // never a store per call (a cell rewritten on every miss cost
                // R18MhresidIface `poly` 247 -> 294 ns). Two threads racing on a
                // write each store an accepted class, so either value is sound.
                // The receiver is read again after the shadow reload, as any
                // later consumer reads it. R11 is the per-opcode scratch; RAX is
                // rewritten to the `1` it held.
                // JIT round 21 (lane typecheck; correctness, unconditional): a
                // way is written only for a receiver class whose supers
                // display carries the hierarchy-only verdict. The helper's `1`
                // is a property of the CLASS only then: for an annotation
                // proxy (one shared class id for every annotation type), a
                // `Proxy$Instance` fallback or an `Unmodifiable*` stamp it is
                // read from the INSTANCE (`jit_typecheck_resolve`'s
                // `instance_admitted` arms), and a way holding such a class
                // answered `1` for the next instance of it, whatever that
                // instance implements. All of them carry verdict `0`
                // (`vm::jit::helpers::typecheck_miss_is_hierarchy_only`). No
                // display cell (the displays switched off): no way is written.
                let verdict_cell = self.class_assignable_display_cell();
                if let (true, Some(cid), Some(verdict_cell)) =
                    (seen_cell != 0, target_class_id, verdict_cell)
                {
                    self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
                    let skip_zero = self.emit_jcc_rel32(0x84); // JZ -> join
                    self.gp_load_value(R11, obj);
                    // TEST BYTE [R11+KIND_TAGS_BYTE_OFFSET], KIND_TAG_BYTE_MASK ; JNE -> keep
                    self.buf.emit(&[
                        0x41,
                        0xF6,
                        0x43,
                        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                        cratonvm_types::KIND_TAG_BYTE_MASK,
                    ]);
                    let skip_array = self.emit_jcc_rel32(0x85);
                    self.buf.emit(&[0x45, 0x8B, 0x1B]); // MOV R11D, [R11]
                    self.buf.emit(&[0x45, 0x85, 0xDB]); // TEST R11D, R11D
                    let skip_synthetic = self.emit_jcc_rel32(0x88); // JS -> keep
                    // The receiver class's display verdict word (the layout
                    // `typecheck_display_answer_bytes` reads): table, capacity,
                    // display, then `[len][chain][n][ifaces][verdict]`.
                    self.note_rcx_written();
                    self.emit_mov_reg_imm64(RAX, verdict_cell as u64);
                    self.buf.emit(&[0x48, 0x8B, 0x00]); // MOV RAX, [RAX] (table)
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    let no_table = self.emit_jcc_rel32(0x84); // JZ -> keep
                    self.buf.emit(&[0x44, 0x89, 0xD9]); // MOV ECX, R11D (zero-extends)
                    self.buf.emit(&[0x48, 0x3B, 0x08]); // CMP RCX, [RAX] (capacity)
                    let past_table = self.emit_jcc_rel32(0x83); // JAE -> keep
                    self.buf.emit(&[0x48, 0x8B, 0x44, 0xC8, 0x08]); // MOV RAX, [RAX + RCX*8 + 8]
                    self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
                    let no_display = self.emit_jcc_rel32(0x84); // JZ -> keep
                    self.buf.emit(&[0x8B, 0x08]); // MOV ECX, [RAX] (len)
                    self.buf.emit(&[0x48, 0x8D, 0x44, 0x88, 0x04]); // LEA RAX, [RAX + RCX*4 + 4] (-> n)
                    self.buf.emit(&[0x8B, 0x08]); // MOV ECX, [RAX] (n)
                    self.buf.emit(&[0x83, 0x7C, 0x88, 0x04, 0x01]); // CMP DWORD [RAX + RCX*4 + 4], 1
                    let not_hierarchy = self.emit_jcc_rel32(0x85); // JNE -> keep
                    self.emit_mov_reg_imm64(RAX, seen_cell as u64);
                    // CMP DWORD [RAX], cid ; JNE -> way 1 ; MOV [RAX], R11D ;
                    // JMP -> keep.
                    self.buf.emit(&[0x81, 0x38]);
                    self.buf.emit(&cid.to_le_bytes());
                    let way1 = self.emit_jcc_rel32(0x85);
                    self.buf.emit(&[0x44, 0x89, 0x18]);
                    let written0 = self.emit_jmp_rel32();
                    self.patch_rel32_to_here(way1);
                    // CMP DWORD [RAX+4], cid ; JNE -> keep (both written) ;
                    // CMP [RAX], R11D ; JE -> keep (way 0's class, a racing
                    // writer) ; MOV [RAX+4], R11D.
                    self.buf.emit(&[0x81, 0x78, 0x04]);
                    self.buf.emit(&cid.to_le_bytes());
                    let full = self.emit_jcc_rel32(0x85);
                    self.buf.emit(&[0x44, 0x39, 0x18]);
                    let same = self.emit_jcc_rel32(0x84);
                    self.buf.emit(&[0x44, 0x89, 0x58, 0x04]);
                    for p in [
                        skip_array,
                        skip_synthetic,
                        no_table,
                        past_table,
                        no_display,
                        not_hierarchy,
                        written0,
                        full,
                        same,
                    ] {
                        self.patch_rel32_to_here(p);
                    }
                    // MOV EAX, 1
                    self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]);
                    self.patch_rel32_to_here(skip_zero);
                }
                // JIT r25 (`CRATONVM_JIT_IR_INSTANCEOF_PROXY_WAYS`): the
                // helper's answer for a plain lambda-proxy receiver becomes
                // the matching way's class (way 0 for a `1`, way 1 for a
                // `0`), last-answer-wins; only the helper path writes, so a
                // site answered inline never stores. A racing writer stores
                // another proxy the helper answered the same way, so either
                // value is sound. The receiver is read again after the
                // shadow reload; RAX is rewritten to the answer it held, and
                // RCX is not touched.
                // MOV R11, obj ; TEST BYTE [R11+KIND_TAGS], MASK ; JNE keep ;
                // MOV R11D, [R11] ; TEST R11D, R11D ; JNS keep ;
                // TEST EAX, EAX ; JZ miss ;
                // MOV RAX, cell ; MOV [RAX], R11D ; MOV EAX, 1 ; JMP keep ;
                // miss: MOV RAX, cell ; MOV [RAX+4], R11D ; XOR EAX, EAX ; keep:
                // Round rx27 (`CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS`): the hit
                // store is `XCHG [RAX], R11D ; MOV [RAX+8], R11D`
                // ([`proxy_hit_way_fill_bytes`]).
                if proxy_cell != 0 && target_class_id.is_some() {
                    self.gp_load_value(R11, obj);
                    self.buf.emit(&[
                        0x41,
                        0xF6,
                        0x43,
                        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
                        cratonvm_types::KIND_TAG_BYTE_MASK,
                    ]);
                    let skip_array = self.emit_jcc_rel32(0x85);
                    self.buf.emit(&[0x45, 0x8B, 0x1B]); // MOV R11D, [R11]
                    self.buf.emit(&[0x45, 0x85, 0xDB]); // TEST R11D, R11D
                    let skip_hierarchy = self.emit_jcc_rel32(0x89); // JNS -> keep
                    self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
                    let to_miss = self.emit_jcc_rel32(0x84); // JZ -> miss
                    self.emit_mov_reg_imm64(RAX, proxy_cell as u64);
                    self.emit_proxy_hit_way_fill();
                    self.buf.emit(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // MOV EAX, 1
                    let hit_written = self.emit_jmp_rel32();
                    self.patch_rel32_to_here(to_miss);
                    self.emit_mov_reg_imm64(RAX, proxy_cell as u64);
                    self.buf.emit(&[0x44, 0x89, 0x58, PROXY_WAY_MISS_DISP]); // MOV [RAX+4], R11D
                    self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
                    for p in [skip_array, skip_hierarchy, hit_written] {
                        self.patch_rel32_to_here(p);
                    }
                }
                // The inline answers join here with 0/1 in RAX, exactly as
                // the helper leaves it, so the arm keeps its one home write.
                for p in answered {
                    self.patch_rel32_to_here(p);
                }
                self.store_rax(slot);
            }
            // ── cov-01: getstatic ────────────────────────────────────────
            //
            // Two lowerings, chosen by the same predicate the single-pass
            // backend's `0xb2` arm uses (`x64::try_emit_inline_getstatic`), and
            // deliberately not by a second opinion:
            //
            //   * DIRECT — `resolve_static_base` accepted the site, which it
            //     does only for a class already initialised at compile time and
            //     published in the lock-free `StaticsIndex`. What is baked is
            //     the address of the class's base-POINTER cell, never of the
            //     block: the one extra dependent load is what buys immunity to
            //     every republication path. An inline load runs no `<clinit>`,
            //     which is sound only because the class is already initialised
            //     AND `CompiledMethod::static_init_classes` re-checks at the
            //     compiled entry.
            //   * HELPER — everything the resolver declines: a class not yet
            //     initialised, `java/lang/System`'s bootstrap intercept,
            //     anything not yet published, and every site when
            //     `CRATONVM_JIT=getstatic-helper` is set. `jit_getstatic` runs
            //     `<clinit>` on first touch and, on failure, stashes the Java
            //     exception and returns the `i64::MIN` deopt sentinel — routed
            //     through the shared exception epilogue rather than pushed as
            //     if it were a field value.
            //
            // A volatile static takes NO fence after the read (round 9 wave 2):
            // x86-64 already gives the load itself acquire ordering; the
            // `CRATONVM_JIT_VOLATILE_LOAD_FENCE=1` kill switch restores the
            // `MFENCE` on both routes.
            Op::LoadStatic {
                class_id,
                field_index,
                type_tag,
                is_volatile,
            } => {
                let (class_id, field_index, type_tag, is_volatile) =
                    (*class_id, *field_index, *type_tag, *is_volatile);
                if self.emit_inline_getstatic(id, class_id, field_index, type_tag, is_volatile) {
                    return;
                }
                // JIT round 21 wave 1 (lane indy, INDY21-2): the class-init
                // barrier's inline read for the claimant thread, AHEAD of the
                // map (whose shadow push only the post-call reload pops), with
                // the helper route below as its decline edge.
                let clinit_barrier_done = self.emit_clinit_barrier_getstatic(
                    id,
                    class_id,
                    field_index,
                    type_tag,
                    is_volatile,
                );
                // The helper can run `<clinit>`, i.e. arbitrary Java. Same
                // pre-call map obligation as `Op::Call`, and an owed
                // volatile-store fence is paid on this route
                // (`fence_sink_transparent`).
                self.emit_owed_fence_before_helper_call();
                let sp_live_hi = self.spill_high_water;
                let mut map = self.emit_safepoint_map(sp_live_hi);
                let slot = self.alloc_slot(id);
                self.load_context(CALL_ARG_REGS[0]);
                self.emit_mov_reg_imm64(CALL_ARG_REGS[1], u64::from(class_id));
                self.emit_mov_reg_imm64(CALL_ARG_REGS[2], u64::from(field_index));
                self.emit_mov_reg_imm64(RAX, self.getstatic as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
                                              // The inline-frame row belongs at the return address, which the
                                              // optional fence below would move `buf` past (round 11 wave 2,
                                              // lane lower); hence `_after_note` further down.
                self.note_inline_frame_return_site();
                if is_volatile && self.volatile_load_fence_on {
                    // Off by default since round 9 wave 2: x86-TSO already gives
                    // a load acquire ordering, and the JMM's StoreLoad is the
                    // volatile store's fence (see
                    // `runtime_lowering::volatile_load_fence_enabled`). When the
                    // kill switch restores it, it goes here rather than after
                    // the sentinel check because the check may CALL
                    // `dispatch_threw` for a wide value; this position is before
                    // any branch, so the fence is on every path out of the load.
                    // `MFENCE` touches neither RAX nor the flags.
                    self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                }
                // `i64::MIN` = the helper published a pending Java exception (a
                // failed `<clinit>`) instead of a value — but for a `J`/`D`/`F`
                // static a legitimate `Long.MIN_VALUE` is bit-identical to it,
                // so the sentinel alone is ambiguous. `emit_call_return_check`
                // is exactly that protocol: plain compare-and-bail for the
                // unambiguous widths, and for a wide value the cold-path peek at
                // `jit_dispatch_threw` that distinguishes a real signal from a
                // real value.
                //
                // It is the SAME helper and the SAME disambiguation the
                // single-pass `0xb2` arm has used for wide statics all along —
                // its `emit_post_invoke_exception_check(type_tag)` takes the
                // `J`/`D`/`F` branch — so this is matching that arm's behaviour
                // rather than inventing one. It also folds in the post-call
                // frame republish and shadow reload this arm was open-coding.
                self.emit_call_return_check_after_note(slot, node.ty);
                // An FP result arrives as bits in RAX and has just been stored
                // to its home word by the line above; publish it to its register
                // the way `Op::ConstF` does. A no-op when the allocator gave
                // this value no register.
                if matches!(type_tag, b'F' | b'D') {
                    self.publish_fp_from_slot(id, slot, type_tag == b'D');
                }
                if let Some(done) = clinit_barrier_done {
                    self.patch_rel32_to_here(done);
                }
            }
            // putstatic: the helper call, always (see `Op::StoreStatic`) --
            // `set_static_shared` owns the SATB pre-barrier and first-touch
            // block creation. The helper can run `<clinit>`, so the pre-call
            // map and the post-call record/reload are the `Op::LoadStatic`
            // helper route's, and the arguments are loaded AFTER the map (its
            // shadow push clobbers the argument registers). It answers 0 or
            // the sentinel, never a value, so the plain `JO` check serves every
            // width. A volatile write then owes the StoreLoad fence, the same
            // `LOCK ADD` a volatile `Op::Store` gets in `lower_block`.
            Op::StoreStatic {
                class_id,
                field_index,
                type_tag,
                is_volatile,
            } => {
                let (class_id, field_index, type_tag, is_volatile) =
                    (*class_id, *field_index, *type_tag, *is_volatile);
                // JIT round 21 (lane promote, TYPECHECK21-9): an in-loop store
                // of a promoted static cell kept only for its position and its
                // value's liveness (`Graph::promoted_sunk_stores`), as the
                // `Op::Store` arm's: it emits nothing; the exit and every
                // deopt pad of the loop write the cell.
                if self.is_silent_promoted_store(id) {
                    return;
                }
                let Some(&value) = node.inputs.get(2) else {
                    self.latch_bailout(Bailout::new(BailoutReason::UnsupportedShape(
                        "ir_lower: Op::StoreStatic without its value input",
                    )));
                    return;
                };
                // Round 16 (lane fieldguard): a primitive static of an
                // initialized class is a direct store (no call, no map, no
                // sentinel); see `emit_inline_putstatic`. Off by default.
                if self.emit_inline_putstatic(class_id, field_index, type_tag, value) {
                    if is_volatile {
                        self.emit_or_defer_store_fence();
                    }
                    return;
                }
                // JIT r20 wave 17 (lane pAlloc2): a REFERENCE static stores
                // inline while the SATB pre-gate is clear; an armed gate takes
                // the helper below, which logs the old value. `Some(join)` is
                // the fast path's jump past the helper route.
                let ref_fast_join =
                    self.emit_inline_putstatic_ref(class_id, field_index, type_tag, value);
                let helper = match type_tag {
                    b'J' => self.putstatic[1],
                    b'F' => self.putstatic[2],
                    b'D' => self.putstatic[3],
                    b'L' | b'[' => self.putstatic[4],
                    _ => self.putstatic[0],
                };
                // The helper route is a call: an owed fence is paid on it.
                self.emit_owed_fence_before_helper_call();
                let sp_live_hi = self.spill_high_water;
                let mut map = self.emit_safepoint_map(sp_live_hi);
                self.load_context(CALL_ARG_REGS[0]);
                self.emit_mov_reg_imm64(CALL_ARG_REGS[1], u64::from(class_id));
                self.emit_mov_reg_imm64(CALL_ARG_REGS[2], u64::from(field_index));
                // An FP value rides a GPR as its raw bits, as for `Op::Store`'s
                // wide helper route; the helper reads the low 32 for `F`/`I`.
                self.gp_load_value(CALL_ARG_REGS[3], value);
                self.emit_mov_reg_imm64(RAX, helper as u64);
                self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
                self.note_inline_frame_return_site();
                self.emit_post_call_frame_record();
                self.emit_shadow_reload();
                self.emit_call_return_sentinel_tail(IrType::Int);
                if let Some(join) = ref_fast_join {
                    self.patch_rel32_to_here(join);
                }
                if is_volatile {
                    self.emit_or_defer_store_fence();
                }
            }
            // ── FP value tier (inc 30) ───────────────────────────────────
            // A float/double constant is just its IEEE bit pattern written to
            // the result slot via a GPR immediate — no XMM. A float's payload
            // sits in the low 32 bits (high 32 left zero by the imm32 form);
            // every consumer reads it back with MOVSS (4 bytes).
            Op::ConstF(bits) => {
                let slot = self.alloc_slot(id);
                self.emit_mov_rax_imm64(*bits as i64);
                self.store_rax(slot);
                // A float/double constant is materialised as an integer
                // immediate, so its register copy comes from the home word.
                // `regalloc` calls `Op::ConstF` rematerializable and evicts it
                // first for exactly this reason; here it is simply cheap.
                self.publish_fp_from_slot(id, slot, node.ty == IrType::Double);
            }
            // int → float / double. Load the int operand to EAX and convert.
            //
            // Round 11 (irlower): `CVTSI2SS/SD` writes only the low lane of
            // XMM0 and so carries a false dependency on XMM0's previous writer
            // -- in an FP loop that is usually the last `DIVSD`/`MULSD` of the
            // iteration, which serialises the conversion behind it. The
            // `XORPS XMM0, XMM0` zeroing idiom (dependency-breaking on every
            // x86-64 core; HotSpot emits the same) cuts it. XMM0 is this tier's
            // scratch and holds no value between nodes, and the source is RAX.
            Op::I2F => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                self.buf.emit(&[0x0F, 0x57, 0xC0]); // XORPS XMM0, XMM0
                // CVTSI2SS XMM0, EAX
                self.buf.emit(&[0xF3, 0x0F, 0x2A, 0xC0]);
                self.fp_store_value(id, slot, XMM0, false);
            }
            Op::I2D => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                self.buf.emit(&[0x0F, 0x57, 0xC0]); // XORPS XMM0, XMM0
                // CVTSI2SD XMM0, EAX
                self.buf.emit(&[0xF2, 0x0F, 0x2A, 0xC0]);
                self.fp_store_value(id, slot, XMM0, true);
            }
            // long → float / double (64-bit source operand in RAX).
            Op::L2F => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                self.buf.emit(&[0x0F, 0x57, 0xC0]); // XORPS XMM0, XMM0
                // CVTSI2SS XMM0, RAX (REX.W)
                self.buf.emit(&[0xF3, 0x48, 0x0F, 0x2A, 0xC0]);
                self.fp_store_value(id, slot, XMM0, false);
            }
            Op::L2D => {
                let slot = self.alloc_slot(id);
                self.gp_load_value(RAX, node.inputs[0]);
                self.buf.emit(&[0x0F, 0x57, 0xC0]); // XORPS XMM0, XMM0
                // CVTSI2SD XMM0, RAX (REX.W)
                self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2A, 0xC0]);
                self.fp_store_value(id, slot, XMM0, true);
            }
            // float → int / long (truncate toward zero, with the JVM
            // NaN→0 / overflow→MAX|MIN fixup). Source stays in XMM0 for the
            // fixup's sign/NaN test.
            Op::F2I => {
                let slot = self.alloc_slot(id);
                self.fp_load_value(XMM0, node.inputs[0], false);
                // CVTTSS2SI EAX, XMM0
                self.buf.emit(&[0xF3, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_fixup(/* is_double */ false, /* is_long */ false);
                // Sign-extend EAX→RAX so the int slot matches the single-pass ABI.
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                self.store_rax(slot);
            }
            Op::F2L => {
                let slot = self.alloc_slot(id);
                self.fp_load_value(XMM0, node.inputs[0], false);
                // CVTTSS2SI RAX, XMM0 (REX.W)
                self.buf.emit(&[0xF3, 0x48, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_fixup(/* is_double */ false, /* is_long */ true);
                self.store_rax(slot);
            }
            // float → double.
            Op::F2D => {
                let slot = self.alloc_slot(id);
                self.fp_load_value(XMM0, node.inputs[0], false);
                // CVTSS2SD XMM0, XMM0
                self.buf.emit(&[0xF3, 0x0F, 0x5A, 0xC0]);
                self.fp_store_value(id, slot, XMM0, true);
            }
            // double → int / long (truncate toward zero, with the JVM fixup).
            Op::D2I => {
                let slot = self.alloc_slot(id);
                self.fp_load_value(XMM0, node.inputs[0], true);
                // CVTTSD2SI EAX, XMM0
                self.buf.emit(&[0xF2, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_fixup(/* is_double */ true, /* is_long */ false);
                self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                self.store_rax(slot);
            }
            Op::D2L => {
                let slot = self.alloc_slot(id);
                self.fp_load_value(XMM0, node.inputs[0], true);
                // CVTTSD2SI RAX, XMM0 (REX.W)
                self.buf.emit(&[0xF2, 0x48, 0x0F, 0x2C, 0xC0]);
                self.emit_fp_to_int_fixup(/* is_double */ true, /* is_long */ true);
                self.store_rax(slot);
            }
            // double → float.
            Op::D2F => {
                let slot = self.alloc_slot(id);
                self.fp_load_value(XMM0, node.inputs[0], true);
                // CVTSD2SS XMM0, XMM0
                self.buf.emit(&[0xF2, 0x0F, 0x5A, 0xC0]);
                self.fp_store_value(id, slot, XMM0, false);
            }
            // Control and meta nodes — skip. `Op::Throw` is a terminator like
            // `Op::Return`/`Op::If`, handled by `lower_terminator`; it never
            // reaches this function from a real schedule (`Op::is_control()`
            // routes it away from ordinary data-node placement).
            Op::Start
            | Op::Return
            | Op::Throw
            | Op::If
            | Op::Switch { .. }
            | Op::Merge
            | Op::Region
            | Op::Proj(_)
            | Op::Dead => {}
            // No lowering arm. REFUSE the compile; do NOT fall through.
            //
            // This arm used to be `_ => {}` with the comment "bail in
            // `ir_compatible` prevents reaching here". That claim was asserted,
            // never checked, and it cannot be checked where it was written:
            // `ir::ir_compatible` decides admission in the BYTECODE's
            // vocabulary (`scan.anewarray_ops`, `scan.typecheck_ops`,
            // `scan.has_athrow`), and this match is over `ir::Op`. Two
            // enumerations of different things, kept in agreement by a comment.
            //
            // What silence costs is not an optimization, it is the node's
            // semantics. `ir::Op::MonitorEnter` is the worked example: when the
            // monitor ops were added to the IR for escape analysis, this
            // catch-all would have compiled a `monitorenter` to *nothing* — the
            // lock silently gone, an unbalanced `monitorexit` left behind. That
            // one op got a hand-written guard at the top of
            // `lower_inner_with_array_list`; this arm is the general form of it.
            //
            // `verify_data_locations` is NOT that general form, though it looks
            // like it: it refuses when a value-typed input's op is absent from
            // `op_defines_result_slot`, so it only ever fires for an unlowered
            // op whose result someone READS. A monitor produces no value.
            // Nothing reads it, so nothing checked it — which is exactly why
            // that op and not another was the one that could vanish.
            //
            // Costs nothing when the claim it replaces is true. Verified
            // 2026-08-03: the `ir::Op` variants with no arm here were then
            // `I2B`, `I2C`, `I2S`, `NewArray` — all unreachable from a real
            // compile. `I2B`/`I2C`/`I2S` are constructed NOWHERE in the crate
            // (`IrBuilder` decomposes 0x91/0x92/0x93 into `Shl`/`Shr`/`And`
            // instead — see the arms at `ir.rs`'s 0x91). `NewArray` has since
            // gained both a builder arm and a lowering arm (cov-06), so it is
            // no longer one of them (round 9 wave 2: this said it was built
            // only by tests).
            //
            // `ArrayLength` was the fifth until COV-02 gave it both an
            // `arraylength` builder arm and a lowering arm above.
            //
            // Latched rather than returned because this function is infallible
            // by signature and every emitting arm below assumes it stays that
            // way. `lower_inner_with_array_list` takes the latch after the last
            // block and discards the artifact — the same channel `alloc_slot`
            // and `slot_of` already use, and the reason `poison_slot` exists.
            other => {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnsupportedShape("ir_lower: op has no lowering arm"),
                    format!(
                        "n{id} ({other:?}, {:?}) reached lower_data_node's catch-all; \
                         emitting nothing would drop its semantics",
                        node.ty
                    ),
                ));
            }
        }

        // Publish this definition into its assigned GP register once (reads check `gp_reg_live`).
        // Phis already resident via edge copies skip re-publishing to avoid back-edge reload chains.
        if self.assigned_gpr(id).is_some()
            && !(ir_skip_live_republish_enabled() && self.resident_gpr(id).is_some())
        {
            if let Some(off) = self.node_slot.get(id as usize).copied().flatten() {
                let slot = off.get() as i32;
                self.publish_gp_from_slot(id, slot);
            }
        }
    }
}

/// Does a call row of `invoke_kind` take its site's inline-cache pair
/// (`emit_inline_cache_call`) when the planner gave it one? Virtual (0),
/// interface (2), and statically bound instance (1): a kind-1 site holds a pair
/// only as a bound-site cache (`ir_bound_site_ic_planned`,
/// `ir_splice_unbound_call_takes_ic`), whose miss helper's bound-site arm
/// enters the statically bound target. A static row (3) never does: a folded
/// constant handle's box call keeps the `invokeExact`'s pc and its pair, and
/// has no receiver to key it. 2026-11-16: kind 1 had been left out since the
/// fold landed, so every unbound private callee (`Method.isCallerSensitive`,
/// `AbstractStringBuilder.appendNull`) was a by-name dispatch on every call and
/// the acceptance gate refused its caller's optimizing body.
pub(in crate::ir_lower) fn ir_ic_cascade_admits_kind(invoke_kind: u8) -> bool {
    matches!(invoke_kind, 0..=2)
}

#[cfg(test)]
mod ic_cascade_kind_tests;
