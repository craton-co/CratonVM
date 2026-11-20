#[allow(unused_imports)]
use super::*;

impl Arm64Backend {
    /// `putfield` of a REFERENCE field, round 9 wave 19.
    ///
    /// ```text
    ///   CBZ   Xobj, npe_NONE
    ///   LDR   Xctx, [FP, #context]
    ///   DMB   ISH                         ; volatile only
    ///   <emit_helper_call(jit_putfield_object, [ctx, obj, field_index, val])>
    ///   DMB   ISH                         ; volatile only
    /// ```
    ///
    /// # What the helper buys, and why nothing here is inline
    ///
    /// A reference store owes two barriers this backend emits nowhere: the
    /// SATB pre-write barrier (log the OLD value, or a concurrent marker loses
    /// the only path to a still-live object — a use-after-free on the next
    /// evacuation) and the card write-barrier that keeps the remembered set
    /// right. It also owes whatever ENCODING the collector is using: a
    /// compressed-oops slot is a 4-byte `(addr - base) >> 3`, and an armed ZGC
    /// slot is a coloured word that is not an address. `jit_putfield_object`
    /// does all of it; x64 has inline fast paths for the cases it can prove
    /// barrier-free and falls back to this same helper otherwise.
    ///
    /// # Not a safepoint
    ///
    /// The barriers enqueue and mark; they run no Java and allocate nothing on
    /// the Java heap, so nothing here can move an object. x64 records no oop
    /// map at this call either — unlike its `putstatic_object` sibling, which
    /// gets one because `<clinit>` can run inside it. This is the
    /// per-helper judgement [`Self::emit_helper_call`] asks every caller to
    /// make rather than assume.
    ///
    /// The null check and the fences are the primitive path's, unchanged: the
    /// helper's own receiver guard returns WITHOUT raising, and a `volatile`
    /// reference field is as volatile as a `volatile` `int`.
    pub(in crate::aarch64_backend) fn emit_putfield_reference(&mut self, field: Arm64InstanceField) -> bool {
        if self.helpers.putfield_object == 0 || !self.can_throw_npe() {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        let Ok(index) = i64::try_from(field.field_index) else {
            return false;
        };
        let val = self.pop_kind(OperandKind::Ref);
        let obj = self.pop_kind(OperandKind::Ref);
        if self.failed {
            return false;
        }
        self.emit_null_check(obj, npe_action::NONE);
        let ctx = self.alloc_reg(false);
        if self.failed {
            return false;
        }
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        if field.is_volatile {
            self.buffer.emit(Arm64Instruction::DmbIsh);
        }
        if !self.emit_helper_call(
            self.helpers.putfield_object,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Reg(obj),
                Arm64HelperArg::Imm(index),
                Arm64HelperArg::Reg(val),
            ],
            None,
        ) {
            return false;
        }
        if field.is_volatile {
            self.buffer.emit(Arm64Instruction::DmbIsh);
        }
        true
    }

    /// `putfield` (`0xb5`) of a PRIMITIVE field, round 9 wave 15 -- the first
    /// lowering on this backend to call a helper from the middle of a method.
    ///
    /// ```text
    ///   CBZ   Xobj, npe_NONE              ; JVMS: a null receiver throws NPE
    ///   FMOV  Xval, Dval                  ; F/D only: the helpers take bits
    ///   <narrow the value to the field's declared type>
    ///   DMB   ISH                         ; volatile only -- see below
    ///   <emit_helper_call(jit_putfield_*, [obj, field_index, val])>
    ///   DMB   ISH                         ; volatile only
    /// ```
    ///
    /// # Why the helper at all, when the store itself is one instruction
    ///
    /// An instance field has TWO layouts per OBJECT -- the packed compact one
    /// and the legacy 16-byte tagged `Value` cell -- chosen at allocation
    /// time, plus a layout epoch the class manager can bump. x64 emits an
    /// inline store with a runtime branch and falls back to this same helper
    /// on every path it declines, one of which is "a compact receiver at a
    /// site with no baked compact offset", where no address is computable at
    /// all. A lowering with no fallback would have to be total for every
    /// layout, and for that receiver it cannot be. The inline fast path is a
    /// later wave; the helper is what makes one possible, because the
    /// fallback has to exist before the branch that needs it.
    ///
    /// # The null check is ours, not the helper's
    ///
    /// `jit_putfield_int`'s receiver guard (`!plausible_heap_pointer(obj_ptr)
    /// -> return`) exists to avoid dereferencing garbage, and it returns
    /// WITHOUT raising: a `putfield` on null through it drops the store and
    /// carries on. x64 emits its own null check ahead of the call for exactly
    /// this reason (`emit_precise_null_check_field_store`, with
    /// `npe_action::NONE` -- JEP 358 has no field-access action code), and so
    /// does this. Hence the [`Self::can_throw_npe`] gate: without an exact NPE
    /// path the method is refused rather than compiled with a `putfield` that
    /// silently does nothing.
    ///
    /// # `DMB ISH` on BOTH sides of a volatile store, which x64 does not owe
    ///
    /// This is the part that does not carry over from the x64 tier. There the
    /// store is a plain `MOV` and TSO makes it a release already, so a
    /// volatile `putfield` owes only the trailing StoreLoad edge (`MFENCE`,
    /// `x64::BackendRequest::volatile_field_pcs`). Here the store happens
    /// INSIDE the helper, where it is a relaxed store
    /// (`write_compact_field(.., Ordering::Relaxed)`), and a `BLR` is not a
    /// barrier on AArch64: without a leading `DMB ISH` the helper's store may
    /// become visible before a Java store that precedes it in program order,
    /// which is precisely the release edge the JMM requires. The trailing one
    /// is the StoreLoad edge, the same one x64 pays.
    ///
    /// The instructions the leading fence ends up preceding -- the operand
    /// spills `emit_helper_call` emits, the argument moves -- are stores to
    /// THIS thread's own frame, which the fence orders harmlessly; what
    /// matters is that every Java-visible access before this bytecode is on
    /// the far side of it.
    ///
    /// A REFERENCE field is not here: it goes to
    /// [`Self::emit_putfield_reference`], which calls the helper that runs the
    /// SATB pre-barrier and the card mark.
    ///
    /// Returns `false`, and the caller refuses the method, for an unwired
    /// helper or no exact NPE path.
    pub(in crate::aarch64_backend) fn emit_putfield(&mut self, field: Arm64InstanceField) -> bool {
        if matches!(field.type_tag, b'L' | b'[') {
            return self.emit_putfield_reference(field);
        }
        let Some(kind) = primitive_field_operand_kind(field.type_tag) else {
            return false;
        };
        let helper = match field.type_tag {
            b'J' => self.helpers.putfield_long,
            b'F' => self.helpers.putfield_float,
            b'D' => self.helpers.putfield_double,
            _ => self.helpers.putfield_int,
        };
        if helper == 0 {
            return false;
        }
        if !self.can_throw_npe() {
            return false;
        }
        let Ok(index) = i64::try_from(field.field_index) else {
            return false;
        };
        // JVMS operand order: the receiver is UNDER the value.
        let val = self.pop_kind(kind);
        let obj = self.pop_kind(OperandKind::Ref);
        if self.failed {
            return false;
        }
        self.emit_null_check(obj, npe_action::NONE);
        // All four helpers take `(obj_ptr: i64, field_index: i64, val: i64)`
        // and reinterpret the bits (`f32::from_bits(val as u32)`), so a float
        // or double argument travels in a GPR, not in V0-V7.
        let val = match kind {
            OperandKind::F32 => {
                let g = self.alloc_reg(false);
                self.buffer
                    .emit(Arm64Instruction::FmovFromFpSingle { rd: g, vn: val });
                g
            }
            OperandKind::F64 => {
                let g = self.alloc_reg(false);
                self.buffer
                    .emit(Arm64Instruction::FmovFromFp { rd: g, vn: val });
                g
            }
            _ => val,
        };
        if self.failed {
            return false;
        }
        self.emit_narrow_to_field_tag(val, field.type_tag);
        if field.is_volatile {
            self.buffer.emit(Arm64Instruction::DmbIsh);
        }
        // No oop map: `jit_putfield_*` allocates nothing, runs no Java and
        // cannot safepoint, so the reference operands this call spills and the
        // reference locals it leaves in X19-X28 are not relocatable behind its
        // back. See `emit_helper_call`, which states that obligation.
        if !self.emit_helper_call(
            helper,
            &[
                Arm64HelperArg::Reg(obj),
                Arm64HelperArg::Imm(index),
                Arm64HelperArg::Reg(val),
            ],
            None,
        ) {
            return false;
        }
        if field.is_volatile {
            self.buffer.emit(Arm64Instruction::DmbIsh);
        }
        true
    }

    /// `getfield` (`0xb4`), round 9 wave 16 -- the first lowering that needs
    /// the VM context, and the first with a sentinel to disambiguate.
    /// Reference fields since h23 (2026-09-22); see below.
    ///
    /// ```text
    ///   CBZ   Xobj, npe_NONE              ; JVMS: a null receiver throws NPE
    ///   LDR   Xctx, [FP, #context]        ; the VM pointer the prologue homed
    ///   <emit_helper_call(jit_getfield, [ctx, obj, field_index]) -> Xv>
    ///   MOVZ/MOVK Xs, #i64::MIN
    ///   CMP   Xv, Xs
    ///   B.NE  join                        ; the common case: a real value
    ///   <J/D only: call dispatch_threw, and keep the value when it says 0>
    ///   <emit_stamp_throw_bci(bci)>       ; so the drain finds the handler
    ///   MOV   X0, #i64::MIN               ; a pending exception -- leave
    ///   B     epilogue
    /// join:
    ///   SXTW/FMOV                         ; into this backend's canonical form
    /// ```
    ///
    /// # Reference fields (h23, 2026-09-22)
    ///
    /// `L` and `[` were refused here until h23, which was two separate
    /// refusals wearing one coat. The HELPER has handled reference fields all
    /// along -- `jit_getfield` takes `GETFIELD_EXPECT_REFERENCE` in its third
    /// argument and returns the oop -- and the operand model has had
    /// `OperandKind::Ref` since wave 10. What was missing was only this arm
    /// passing the flag and marking the result. Both are now done through the
    /// canonical `getfield_index_arg` encoder rather than the hand-written
    /// `field_index as i64` this site used to build, which is precisely the
    /// rot that encoder's doc comment warns about.
    ///
    /// A reference needs no `dispatch_threw` probe: only `J`/`D` can hold
    /// `i64::MIN` legitimately. It also needs no load barrier, for the same
    /// reason the reference `getstatic` arm needs none -- the helper returns
    /// what every collector in this VM lets compiled code hold.
    ///
    /// # Why the read is a helper call and not two loads
    ///
    /// The same reason `putfield`'s store is: an instance field has two
    /// layouts per OBJECT (packed compact, or the legacy 16-byte tagged
    /// `Value` cell) plus a layout epoch, and x64's inline read declines to a
    /// helper on four paths, one of which -- a compact receiver at a site with
    /// no baked compact offset -- has no computable address at all. A lowering
    /// with no fallback would have to be total for every layout, and there it
    /// cannot be.
    ///
    /// # Why the null check is still ours
    ///
    /// `jit_getfield` DOES raise on a null receiver (unlike `jit_putfield_*`,
    /// which silently drops the store) -- it flags the pending NPE and returns
    /// the sentinel. Checking first is not redundant, though: it keeps the
    /// common null out of the helper, and it means the sentinel path below is
    /// reached only by a receiver that is non-null and implausible, or by a
    /// panic inside the helper.
    ///
    /// # The sentinel, and why `J` and `D` need a second call
    ///
    /// `jit_getfield` returns `i64::MIN` for "an exception is pending" -- and
    /// it returns it ONLY after setting one (`set_jit_pending_npe_action_at`
    /// for an implausible receiver, `set_jit_pending_npe` for one that is not
    /// a live object; an out-of-range slot returns 0, not the sentinel). So
    /// leaving through the epilogue with the sentinel here raises an exception
    /// rather than re-running the method, which is why this lowering does not
    /// need the deopt point that `uncommon_trap`-style bails would.
    ///
    /// For an `int`, `boolean`, `byte`, `char`, `short` or `float` field,
    /// `X0 == i64::MIN` is therefore unambiguous: the helper returns
    /// `i as i64` for an `Int` (sign-extended, never `i64::MIN`) and
    /// `f.to_bits() as i64` for a `Float` (zero-extended 32 bits, likewise).
    ///
    /// **`J` and `D` are ambiguous, and getting this wrong is a miscompile
    /// with no crash.** A `long` field holding `Long.MIN_VALUE` returns
    /// exactly `i64::MIN`, and so does a `double` field holding `-0.0`, whose
    /// bit pattern IS `i64::MIN`. Reading either as "an exception is pending"
    /// throws a `NullPointerException` out of a correct program. So on the
    /// (rare) equal branch those two call `dispatch_threw`, which PEEKS every
    /// out-of-band signal and answers 1 only for a genuine one -- the same
    /// helper and the same reason as x64's `J`/`D` call sites. The value is
    /// re-materialized rather than preserved across that call, because on this
    /// branch it is `i64::MIN` by definition.
    ///
    /// Every `alloc_reg` happens BEFORE the branch, so both paths reach `join`
    /// with the same operand model. That is the rule `emit_helper_call`'s doc
    /// states, and this is the lowering it was written for.
    ///
    /// Returns `false`, and the caller refuses the method, for a reference
    /// field (no narrow-oop decode and no operand-stack oop story), an unwired
    /// helper, no exact NPE path, or no context word.
    pub(in crate::aarch64_backend) fn emit_getfield(&mut self, field: Arm64InstanceField) -> bool {
        let is_reference = matches!(field.type_tag, b'L' | b'[');
        let kind = if is_reference {
            OperandKind::Ref
        } else {
            let Some(kind) = primitive_field_operand_kind(field.type_tag) else {
                return false;
            };
            kind        };
        if self.helpers.getfield == 0 || !self.can_throw_npe() {
            return false;
        }
        // `needs_context` is set from the presence of a `0xb4` in the
        // bytecode, so reaching here without a context word would mean the
        // frame and the walk disagree.
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Only `J`/`D` can legitimately BE `i64::MIN` and so need the
        // `dispatch_threw` probe to tell a real value from a pending
        // exception. A reference cannot: `i64::MIN` is not a mapped address
        // on any target this VM runs, and the helper returns 0 for null.
        let ambiguous = matches!(field.type_tag, b'J' | b'D');
        if ambiguous && self.helpers.dispatch_threw == 0 {
            return false;
        }
        // h23: through the ONE encoder, not `field_index as i64` by hand.
        // `GETFIELD_EXPECT_REFERENCE` is what stops the helper handing back
        // the payload of whatever `Value` variant the slot holds -- a
        // reference field punned to a primitive becomes a wild pointer, and
        // this arm is about to push the result as an oop. The receiver proof
        // is left `false`: this backend's null check is not the
        // three-clause plausibility screen x64's `receiver_proven_oop`
        // means, so claiming it would be a lie the helper acts on.
        // `npe_trap_key` is 0 -- JEP 358 messages are an x64-only feature so
        // far -- which is the documented "site not described" value.
        let Ok(slot) = u32::try_from(field.field_index) else {
            return false;
        };
        let Ok(index) = i64::try_from(cratonvm_jit_api::getfield_index_arg(
            slot,
            is_reference,
            false,
            0,
        )) else {
            return false;
        };

        let obj = self.pop_kind(OperandKind::Ref);
        if self.failed {
            return false;
        }
        self.emit_null_check(obj, npe_action::NONE);

        // EVERY allocation, before the branch. See the doc comment.
        let value = self.alloc_reg(false);
        let ctx = self.alloc_reg(false);
        let sentinel = self.alloc_reg(false);
        let probe = if ambiguous {
            Some(self.alloc_reg(false))
        } else {
            None
        };
        if self.failed {
            return false;
        }

        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        // No oop map: `jit_getfield` allocates nothing and runs no Java, so it
        // cannot safepoint. See `emit_helper_call`, which states that
        // obligation rather than assuming it.
        if !self.emit_helper_call(
            self.helpers.getfield,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Reg(obj),
                Arm64HelperArg::Imm(index),
            ],
            Some(value),
        ) {
            return false;
        }

        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: sentinel,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: value,
            rm: sentinel,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Ne,
            label: join,
        });
        if let Some(probe) = probe {
            let leave = self.buffer.new_label();
            if !self.emit_helper_call(self.helpers.dispatch_threw, &[], Some(probe)) {
                return false;
            }
            self.buffer.emit(Arm64Instruction::Cbnz {
                rt: probe,
                label: leave,
            });
            // Not an exception: the `i64::MIN` is this field's real value.
            // Re-materialized rather than preserved, because the call above
            // destroyed `value` -- and on this branch it can only have been
            // `i64::MIN`.
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: value,
                imm: i64::MIN,
            });
            self.buffer.emit(Arm64Instruction::B { label: join });
            self.buffer.bind_label(leave);
        }
        // A pending exception. The interpreter's JIT-return drain raises it
        // and searches THIS method's exception table from the throw-site bci,
        // so the bci has to be stamped first.
        //
        // h23 (2026-09-22): this edge was missed when `can_throw_npe` was
        // widened from `exception_table_empty` to `can_route_exception`. The
        // inline null check above leaves through the NPE stub, which is
        // bci-keyed and stamps; THIS edge -- the helper's own `i64::MIN`,
        // raised for a receiver the inline check passed -- left with whatever
        // bci a previous stamp had written, which in a method with a
        // `try`/`catch` is the wrong-handler bug the stamp exists to prevent.
        // `emit_putfield`/`emit_putfield_reference` need no such call: they
        // ignore the helper's result and their only trap is that same stub.
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);

        match kind {
            OperandKind::Ref => {
                self.push_reg(OperandKind::Ref, value);
                self.mark_top_operand_as_oop();
            }
            OperandKind::I32 => {
                // The helper already returns a sign-extended `int`, so this is
                // a no-op on every value it can produce; it is emitted so the
                // canonical form is established HERE rather than inferred from
                // the helper's `i as i64`.
                self.emit_sxtw(value);
                self.push_reg(OperandKind::I32, value);
            }
            OperandKind::F32 => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFpSingle { vd: v, rn: value });
                self.push_reg(OperandKind::F32, v);
            }
            OperandKind::F64 => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFp { vd: v, rn: value });
                self.push_reg(OperandKind::F64, v);
            }
            // `I64` (the only other kind the table produces).
            _ => self.push_reg(kind, value),
        }
        true
    }

    // -- Reference array elements -------------------------------------------

    /// Ensure the operand `from_top` entries below the top is in a register,
    /// and answer that register — WITHOUT popping it.
    ///
    /// The difference from [`Self::pop_entry`] is what makes a value survive a
    /// call: [`Self::spill_operands_for_call`] stores and reloads exactly the
    /// entries the model still holds, so an operand left on the stack comes
    /// back in the same register on the far side, while one that was popped is
    /// simply destroyed. `aastore` needs both its array and its value across
    /// TWO calls (the type check and the store), which is what this exists
    /// for.
    ///
    /// `None` (with `failed` set) on underflow.
    pub(in crate::aarch64_backend) fn materialize_entry(&mut self, from_top: usize) -> Option<(Arm64Register, Operand)> {
        let Some(depth) = self.operand_stack.len().checked_sub(from_top + 1) else {
            self.failed = true;
            return None;
        };
        let entry = self.operand_stack[depth];
        match entry.loc {
            OperandLoc::Reg(reg) => {
                // HELD, like `pop_entry`'s. Being live on the stack protects
                // this register from `alloc_reg`'s free-register pass but NOT
                // from its victim pass, which spills the deepest
                // register-located entry -- and a caller that has already
                // captured the register would then be holding a name the
                // model no longer agrees with. Holding makes a later
                // allocation refuse instead.
                if !self.held.contains(&reg) {
                    self.held.push(reg);
                }
                Some((reg, entry))
            }
            OperandLoc::Slot(offset) => {
                let reg = self.alloc_reg(entry.kind.is_fp());
                if self.failed {
                    return None;
                }
                self.emit_load_kind(reg, entry.kind, offset);
                self.operand_stack[depth].loc = OperandLoc::Reg(reg);
                Some((reg, entry))
            }
        }
    }

    /// Leave the frame with the `i64::MIN` deopt sentinel when `reg` holds it
    /// — the shape every helper here that signals through the sentinel shares.
    ///
    /// ```text
    ///   MOVZ/MOVK Xs, #i64::MIN
    ///   CMP   Xreg, Xs
    ///   B.NE  join
    ///   MOV   X0, #i64::MIN
    ///   B     epilogue
    /// join:
    /// ```
    ///
    /// `sentinel` is a register the CALLER allocated, for the same reason
    /// `emit_helper_call`'s destination is: this sits between a call and a
    /// join, and an allocation here would happen on one path only.
    pub(in crate::aarch64_backend) fn emit_bail_on_sentinel(&mut self, reg: Arm64Register, sentinel: Arm64Register) {
        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: sentinel,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: reg,
            rm: sentinel,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Ne,
            label: join,
        });
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);    }

    /// Leave the frame when `reg` is NON-ZERO — the shape for a helper whose
    /// result is binary.
    ///
    /// ```text
    ///   CBZ   Xreg, join
    ///   MOV   X0, #i64::MIN
    ///   B     epilogue
    /// join:
    /// ```
    ///
    /// Used only by `aastore`'s element-type check, whose contract is `0` for
    /// a legal store and `i64::MIN` for a refused one and nothing else. That
    /// is what makes `CBZ` as precise as the `CMP` against the sentinel that
    /// [`Self::emit_bail_on_sentinel`] emits, and one register cheaper -- and
    /// the difference matters at this bytecode, which already holds the array,
    /// the index, the value, the length the AIOOBE stub reads, the context and
    /// the verdict at the same time.
    ///
    /// It is NOT interchangeable with the other form. `aaload`'s result is a
    /// REFERENCE, where every non-zero value but the sentinel is a legal one.
    pub(in crate::aarch64_backend) fn emit_bail_if_refused(&mut self, reg: Arm64Register) {
        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::Cbz {
            rt: reg,
            label: join,
        });
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);    }

    /// `aaload` (`0x32`), round 9 wave 19 — the reference element LOAD.
    ///
    /// ```text
    ///   <the wave-13 guards: CBZ arr, npe; unsigned bounds compare, B.HS aioobe>
    ///   LDR   Xctx, [FP, #context]
    ///   <emit_helper_call_at_safepoint(jit_aaload, [ctx, arr, idx]) -> Xv>
    ///   <bail if Xv is the sentinel>
    ///   <push Xv as a REFERENCE, marked as an oop>
    /// ```
    ///
    /// # Why a helper, when every other array load here is one instruction
    ///
    /// Because a reference element is not always a pointer. Under compressed
    /// oops it is a 4-byte `(addr - base) >> 3` with 0 for null; under an
    /// ARMED ZGC cycle it is a COLOURED word — tag bit 63, colour bits
    /// 42..=46, a 42-bit offset — which is not a machine address at all, and
    /// reading it without the load barrier is a read-barrier hole (the
    /// hazard x64's `0x53` arm documents at length for the store side). x64
    /// lowers this inline because its emitters know both encodings; this
    /// backend knows neither, and `jit_aaload` knows both.
    ///
    /// The guards are still OURS, because the helper's own are not exact: it
    /// reports a null array or a bad index through the pending-signal channel,
    /// which surfaces only at method return. Emitting them inline keeps the
    /// `NullPointerException` and the `ArrayIndexOutOfBoundsException` at the
    /// bytecode that caused them.
    ///
    /// Returns `false`, and the caller refuses the method, without the array
    /// exception paths, an unwired helper, a context word or an empty
    /// exception table.
    pub(in crate::aarch64_backend) fn emit_aaload(&mut self) -> bool {
        // `can_access_array` already folds in `can_route_exception` through
        // `can_throw_npe`/`can_throw_aioobe`; no separate check is owed here.
        if !self.can_access_array() {
            return false;
        }
        if self.helpers.aaload == 0 {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };

        let idx = self.pop_kind(OperandKind::I32);
        let arr = self.pop_kind(OperandKind::Ref);
        if self.failed {
            return false;
        }
        // The same guards every other array access emits. The address it
        // computes is discarded: the helper addresses the element itself,
        // because only it knows the element's encoding.
        if !self.emit_array_guards_only(arr, idx, npe_action::ALOAD_OBJECT) {
            return false;
        }

        // EVERY allocation before the branch. See `emit_helper_call`.
        let ctx = self.alloc_reg(false);
        let out = self.alloc_reg(false);
        let sentinel = self.alloc_reg(false);
        if self.failed {
            return false;
        }
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        if !self.emit_helper_call_at_safepoint(
            self.helpers.aaload,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Reg(arr),
                Arm64HelperArg::Reg(idx),
            ],
            Some(out),
            safepoint_id,
        ) {
            return false;
        }
        // `i64::MIN` cannot be a reference -- a heap pointer is under 2^48 --
        // so the sentinel is unambiguous here, unlike a `long` field's.
        self.emit_bail_on_sentinel(out, sentinel);
        self.push_reg(OperandKind::Ref, out);
        self.mark_top_operand_as_oop();
        true
    }

    /// `aastore` (`0x53`), round 9 wave 19 — the reference element STORE, and
    /// the first lowering here to make TWO calls at one bytecode.
    ///
    /// ```text
    ///   <the wave-13 guards: CBZ arr, npe; unsigned bounds compare, B.HS aioobe>
    ///   LDR   Xctx, [FP, #context]
    ///   <call_at_safepoint(jit_aastore_type_check, [ctx, arr, val]) -> Xr>
    ///   <bail if Xr is the sentinel: an ArrayStoreException is pending>
    ///   LDR   Xctx, [FP, #context]        ; the first call destroyed it
    ///   <call_at_safepoint(jit_aastore, [ctx, arr, idx, val])>
    /// ```
    ///
    /// # Why two calls, and why the array and the value are not popped first
    ///
    /// The store itself owes the SATB pre-write barrier, the card mark and
    /// whatever encoding the collector is using — all of which `jit_aastore`
    /// does and none of which this backend emits anywhere. But `jit_aastore`
    /// returns `()`, so a compiled caller cannot see that it refused the
    /// store: its `ArrayStoreException` travels by the pending-signal channel
    /// and would surface only at method return, after this frame had run on.
    /// `jit_aastore_type_check` exists for exactly that — it answers `0` or
    /// the sentinel — and x64's inline lowering calls it first for the same
    /// reason.
    ///
    /// Two calls means the array and the value have to survive the first one.
    /// They are therefore LEFT ON THE OPERAND STACK and reached with
    /// [`Self::materialize_entry`]: the spill-and-reload around a call
    /// preserves exactly the entries the model still holds, so both come back
    /// in the same registers — and, being reference operands, both are named
    /// in the map, so a collector that moves them during the type check hands
    /// the store their new addresses. A popped value would have neither
    /// property.
    ///
    /// Returns `false`, and the caller refuses the method, without the array
    /// exception paths, either helper, a context word or an empty exception
    /// table.
    pub(in crate::aarch64_backend) fn emit_aastore(&mut self) -> bool {
        // `can_access_array` already folds in `can_route_exception` through
        // `can_throw_npe`/`can_throw_aioobe`; no separate check is owed here.
        if !self.can_access_array() {
            return false;
        }
        if self.helpers.aastore == 0 || self.helpers.aastore_type_check == 0 {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };

        // Top down: value, index, array -- and NOT popped. See the doc.
        let Some((val, val_entry)) = self.materialize_entry(0) else {
            return false;
        };
        let Some((idx, idx_entry)) = self.materialize_entry(1) else {
            return false;
        };
        let Some((arr, arr_entry)) = self.materialize_entry(2) else {
            return false;
        };
        if val_entry.kind != OperandKind::Ref
            || idx_entry.kind != OperandKind::I32
            || arr_entry.kind != OperandKind::Ref
        {
            self.failed = true;
            return false;
        }
        if !self.emit_array_guards_only(arr, idx, npe_action::ASTORE_OBJECT) {
            return false;
        }

        // EVERY allocation before the branch. See `emit_helper_call`. Note
        // there is no sentinel register here, unlike `aaload`'s: the type
        // check's result is BINARY by contract (`0` legal, `i64::MIN`
        // refused), so `CBNZ` decides it, and this bytecode needs the
        // register -- it holds the array, the index, the value, the length
        // the AIOOBE stub reads, the context and the verdict at once.
        let ctx = self.alloc_reg(false);
        let verdict = self.alloc_reg(false);
        if self.failed {
            return false;
        }
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        if !self.emit_helper_call_at_safepoint(
            self.helpers.aastore_type_check,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Reg(arr),
                Arm64HelperArg::Reg(val),
            ],
            Some(verdict),
            safepoint_id,
        ) {
            return false;
        }
        self.emit_bail_if_refused(verdict);
        // The type check destroyed X9-X15 and the reload put back only what
        // the operand MODEL holds; the context is not an operand.
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        if !self.emit_helper_call_at_safepoint(
            self.helpers.aastore,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Reg(arr),
                Arm64HelperArg::Reg(idx),
                Arm64HelperArg::Reg(val),
            ],
            None,
            safepoint_id,
        ) {
            return false;
        }
        // Only now: the three entries this bytecode consumed.
        self.drop_top();
        self.drop_top();
        self.drop_top();
        !self.failed
    }

    // -- Throw --------------------------------------------------------------

    /// `athrow` (`0xbf`), round 9 wave 20.
    ///
    /// ```text
    ///   MOV       X0, Xexc
    ///   MOVZ/MOVK X1, #bci
    ///   MOVZ/MOVK X16, #jit_throw_exception
    ///   BLR       X16                 ; returns the i64::MIN deopt sentinel
    ///   B         epilogue            ; X0 already holds it
    /// ```
    ///
    /// The simplest call shape this backend has, and the one it has had since
    /// wave 11: a stub on the way OUT of the frame. Nothing has to survive it,
    /// so there is no spill, no reload and no oop map -- the operand stack is
    /// dead the moment the exception is raised, and the epilogue restores the
    /// callee-saved homes. What is new in wave 20 is only that the arm exists.
    ///
    /// `jit_throw_exception` stashes the exception as pending and answers the
    /// sentinel, which the interpreter's JIT-return drain turns into the
    /// throw; a NULL `exc_ptr` gets the JVMS `athrow`-on-null
    /// `NullPointerException` from the helper itself, so no inline check is
    /// owed. The `bci` argument is what lets the drain place the throw.
    ///
    /// Unlike every other trapping lowering here, this one needs no
    /// [`Self::can_route_exception`] check: `jit_throw_exception`'s own `bci`
    /// argument reaches `set_jit_pending_exception_with_bci`
    /// (`vm/src/jit/helpers.rs`), the same TLS cell [`Self::emit_stamp_throw_bci`]
    /// writes for everything else, so the drain always has an exact throw
    /// site to route through this method's own exception table -- table empty
    /// or not.
    ///
    /// No fence: `athrow` publishes nothing. The exception object was already
    /// published by whatever created it.
    pub(in crate::aarch64_backend) fn emit_athrow(&mut self) -> bool {
        if self.helpers.throw_exception == 0 {
            return false;
        }
        let Ok(bci) = i64::try_from(self.cur_bytecode_pc) else {
            return false;
        };
        let exc = self.pop_kind(OperandKind::Ref);
        if self.failed {
            return false;
        }
        self.buffer.emit(Arm64Instruction::Mov {
            rd: Arm64Register::X0,
            rm: exc,
        });
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X1,
            imm: bci,
        });
        // Cast: a helper address is a real mapped pointer, always < i64::MAX.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.throw_exception as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });
        // The helper RETURNS the sentinel in X0, so unlike the NPE stub this
        // does not have to materialize one.
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.reachable = false;
        true
    }

    /// `multianewarray` (`0xc5`) at ANY arity, h23c (2026-09-22).
    ///
    /// ```text
    ///   LDR   Xctx, [FP, #context]
    ///   SUB   Xdims, FP, #|offset of the deepest dimension's slot|
    ///   <emit_helper_call_at_safepoint_consuming(multianewarray_n,
    ///        [ctx, site, ndims, dims], consumed = ndims) -> Xout>
    ///   CBNZ  Xout, join               ; 0 = a pending exception
    ///   <emit_stamp_throw_bci(bci)>
    ///   MOV   X0, #i64::MIN
    ///   B     epilogue
    /// join:
    ///   <push Xout as a REFERENCE, marked as an oop>
    /// ```
    ///
    /// # The dimension buffer is the OPERAND AREA, and costs nothing
    ///
    /// x64 has to copy its dimension counts into frame scratch words before
    /// this call, because its operand stack is register-cached and its
    /// argument registers are four wide. Neither is true here. The helper
    /// reads `dims_ptr[0..ndims]` outermost-first from ascending addresses;
    /// this frame's operand words are indexed by depth and ascend in address
    /// with it (see the module header); and JVMS pushes a `multianewarray`'s
    /// dimensions in source order, so the OUTERMOST is the deepest of the
    /// `ndims` entries on top of the stack. The deepest one is therefore
    /// already `dims_ptr[0]`, and the spill that
    /// `emit_helper_call_at_safepoint_consuming` performs anyway is what
    /// writes the buffer.
    ///
    /// So the whole lowering is the one `SUB` that turns a frame offset into
    /// an address -- the same computation `emit_invoke` makes for
    /// `jit_invoke_dispatch`'s argument buffer, for the same reason.
    ///
    /// `consumed = ndims` is what makes the map exact: the entries are named
    /// by the oop map (a dimension is an `int`, but an entry BELOW them may be
    /// a reference), dropped from the model after it is taken, and not
    /// reloaded afterwards.
    ///
    /// # Arity
    ///
    /// Bounded by `MAX_JIT_MULTIANEWARRAY_DIMS`, which is `jit_scan`'s own
    /// bound, so a site that reached here with a wider arity would be one the
    /// scanner should already have refused. Checked rather than asserted: a
    /// `debug_assert!` is no guard in a release build, and the failure mode --
    /// consuming the wrong number of entries -- leaves the operand model out
    /// of step rather than faulting.
    pub(in crate::aarch64_backend) fn emit_multianewarray(&mut self, ndims: usize) -> bool {
        if self.helpers.multianewarray_n == 0 {
            return false;
        }
        // The helper answers 0 with a pending exception (a negative dimension,
        // OOM, a failed resolution), and that leaves through the epilogue.
        if !self.can_route_exception() {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };
        let Some(&site) = self.multianewarray_sites.get(&self.cur_bytecode_pc) else {
            return false;
        };
        if ndims == 0 || ndims > crate::x64::MAX_JIT_MULTIANEWARRAY_DIMS {
            return false;
        }
        let Some(surviving) = self.operand_stack.len().checked_sub(ndims) else {
            return false;
        };
        // Cast: bounded by MAX_JIT_MULTIANEWARRAY_DIMS just above.
        let Ok(ndims_imm) = i64::try_from(ndims) else {
            return false;
        };

        // EVERY allocation before the branch. See `emit_helper_call`.
        let ctx = self.alloc_reg(false);
        let dims_ptr = self.alloc_reg(false);
        let out = self.alloc_reg(false);
        if self.failed {
            return false;
        }

        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        let Some(off) = self.spill_offset_for_depth(surviving) else {
            self.failed = true;
            return false;
        };
        // Frame offsets are negative from FP; the buffer grows upward in
        // address from this one word, exactly as `emit_invoke`'s does.
        let Some(magnitude) = off.checked_neg() else {
            self.failed = true;
            return false;
        };
        self.buffer.emit(Arm64Instruction::SubImm {
            rd: dims_ptr,
            rn: Arm64Register::FP,
            imm: magnitude,
        });

        cratonvm_jit_api::assert_helper_call_shape!(
            "multianewarray_n",
            int_args = 4,
            returns_value = true
        );
        if !self.emit_helper_call_at_safepoint_consuming(
            self.helpers.multianewarray_n,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Imm(site),
                Arm64HelperArg::Imm(ndims_imm),
                Arm64HelperArg::Reg(dims_ptr),
            ],
            Some(out),
            safepoint_id,
            ndims,
        ) {
            return false;
        }

        // 0 (null) means the helper published a pending exception. Unlike the
        // `getfield` family there is no ambiguity to resolve: a successful
        // allocation never answers null.
        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::Cbnz {
            rt: out,
            label: join,
        });
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);

        self.push_reg(OperandKind::Ref, out);
        self.mark_top_operand_as_oop();
        true
    }

    // -- Monitors -----------------------------------------------------------

    /// `monitorenter` (`0xc2`) and `monitorexit` (`0xc3`), h23 (2026-09-22) —
    /// the fifth page's "monitors are dead code" item.
    ///
    /// ```text
    ///   LDR   Xctx, [FP, #context]
    ///   <emit_helper_call_at_safepoint(jit_monitor_enter|exit, [ctx, obj]) -> Xr>
    ///   MOVZ/MOVK Xs, #i64::MIN
    ///   CMP   Xr, Xs
    ///   B.NE  join
    ///   <emit_stamp_throw_bci(bci)>
    ///   MOV   X0, #i64::MIN            ; a pending exception -- leave
    ///   B     epilogue
    /// join:
    /// ```
    ///
    /// # Why a helper call and not an inline lock-word CAS
    ///
    /// Because that is what the other tier that lowers these does.
    /// `ir_lower`'s `Op::MonitorEnter`/`Op::MonitorExit` arm is this same
    /// shape — publish the map, call `jit_monitor_enter`/`jit_monitor_exit`,
    /// test the sentinel — and the thin-lock CAS lives inside the helper,
    /// where the contended path, the inflation and the JVMTI accounting are.
    /// An inline uncontended fast path with the helper as its fallback is a
    /// real optimisation and belongs with the other §4 items on
    /// `aarch64-backend-runs-no-java-on-a-real-machine-20260922.md`; it is not
    /// what "monitors are unwritten" meant, and doing it first would have
    /// meant writing the hard half before the correct half.
    ///
    /// This is also why [`ARM64_LOWERS_EXCLUSIVE_ACCESS`] stays `false`: no
    /// `LDAXR`/`STLXR` is emitted here. That constant answers "which ordering
    /// instructions does this backend emit", and the honest answer is still
    /// none of the exclusive ones.
    ///
    /// # Ordering
    ///
    /// The acquire and the release are the helper's, and it is a `BLR` away
    /// behind a full compiler and hardware barrier set by
    /// `monitor_enter_blocking` / the unlock CAS. This arm owes nothing extra
    /// for the same reason the `invoke*` arms do not: no lowering on this
    /// backend ever leaves a fence owed across a call. See
    /// [`opcode_has_ordered_lowering`].
    ///
    /// # The receiver, and the address it may come back as
    ///
    /// `jit_monitor_enter` returns the possibly-REMAPPED object, because a
    /// contended acquire parks this thread and the object can move while it is
    /// parked. `ir_lower` stores that back into the receiver's slot; there is
    /// nothing to store it back INTO here, because `monitorenter` consumes its
    /// operand and this arm has already popped it. That is not a gap:
    ///
    /// * the popped register is dead after the call — nothing reads it again;
    /// * every OTHER live reference to the same object is either an operand,
    ///   which [`Self::emit_helper_call_at_safepoint`] spills and names in the
    ///   map, or a reference local, which `home_reference_locals_for_call`
    ///   homes and `reload_after_safepoint` reloads — both from slots a moving
    ///   collector rewrites. The `synchronized` block's own
    ///   `astore`d copy, which its generated handler unlocks through, is one
    ///   of those locals.
    ///
    /// So the object this frame goes on to use is read back from a slot the
    /// collector maintained, never from the pre-park address.
    ///
    /// # The sentinel
    ///
    /// `i64::MIN` means the helper published a pending Java exception: a null
    /// receiver's `NullPointerException` for `enter`, an
    /// `IllegalMonitorStateException` for an `exit` this thread does not own.
    /// Unambiguous, unlike a `J`/`D` field read — a reference is never
    /// `i64::MIN`, and `jit_monitor_exit` answers `1` on success. The bci is
    /// stamped before leaving so the drain searches this method's own
    /// exception table from the right site, which for a `synchronized` block
    /// is the whole point: its generated `any` handler is what must run.
    pub(in crate::aarch64_backend) fn emit_monitor_op(&mut self, enter: bool) -> bool {
        let helper = if enter {
            self.helpers.monitor_enter
        } else {
            self.helpers.monitor_exit
        };
        if helper == 0 {
            return false;
        }
        if !self.can_route_exception() {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };

        let obj = self.pop_kind(OperandKind::Ref);
        if self.failed {
            return false;
        }

        // EVERY allocation before the branch. See `emit_helper_call`.
        let ctx = self.alloc_reg(false);
        let out = self.alloc_reg(false);
        let sentinel = self.alloc_reg(false);
        if self.failed {
            return false;
        }

        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        if !self.emit_helper_call_at_safepoint(
            helper,
            &[Arm64HelperArg::Reg(ctx), Arm64HelperArg::Reg(obj)],
            Some(out),
            safepoint_id,
        ) {
            return false;
        }

        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: sentinel,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: out,
            rm: sentinel,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Ne,
            label: join,
        });
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);
        true
    }

    // -- Allocation ---------------------------------------------------------

    /// `new` (`0xbb`), `newarray` (`0xbc`) and `anewarray` (`0xbd`), round 9
    /// wave 18 — the first lowerings that call a helper which can SAFEPOINT.
    ///
    /// ```text
    ///   LDR   Xctx, [FP, #context]
    ///   <emit_helper_call_at_safepoint(alloc helper, args) -> Xobj>
    ///   CBNZ  Xobj, join            ; null means the helper stashed an error
    ///   MOV   X0, #i64::MIN
    ///   B     epilogue
    /// join:
    ///   <push Xobj as a REFERENCE, marked as an oop>
    /// ```
    ///
    /// The three differ only in their helper and their middle argument:
    ///
    /// | opcode | helper | arguments |
    /// | --- | --- | --- |
    /// | `new` | `jit_new_object` | `(vm, class_id, num_fields)` |
    /// | `newarray` | `jit_newarray` | `(vm, atype, length)` |
    /// | `anewarray` | `jit_anewarray_object` | `(vm, component_class_id, length)` |
    ///
    /// `newarray` needs no constant-pool resolution at all: its `atype` is an
    /// immediate operand byte. The other two need the class the caller
    /// resolved into an [`Arm64NewSite`], and refuse a site without one.
    ///
    /// # Why no fence
    ///
    /// The allocation publishes a header, and the JMM's freeze action belongs
    /// to whatever later store makes the reference visible to another thread.
    /// On this backend that store is a `putfield`/`putstatic` through a
    /// helper, which carries its own ordering. Until then the object is
    /// unreachable by construction: nothing but this frame has its address.
    /// x64 emits no fence here either.
    ///
    /// # The null result
    ///
    /// Every one of these helpers returns `0` on failure, having stashed a
    /// pending exception first — `OutOfMemoryError`,
    /// `NegativeArraySizeException`, or whatever a failed `<clinit>` raised.
    /// So leaving through the epilogue with the sentinel THROWS rather than
    /// re-running the method, which is what makes this safe without a deopt
    /// point (the same argument as `getfield` and `putstatic`).
    /// [`Self::emit_stamp_throw_bci`] on that edge is what lets the drain
    /// route it through THIS method's own exception table rather than an
    /// unknown pc. x64 reaches the same place through
    /// `emit_post_alloc_oom_check`; without that check the null is pushed and
    /// the next `arraylength` or field access dereferences it.
    ///
    /// Returns `false`, and the caller refuses the method, for an unwired
    /// helper, an unresolved site, a `set_throw_bci` neither wired nor unneeded
    /// (see [`Self::can_route_exception`]), or no context word.
    pub(in crate::aarch64_backend) fn emit_allocation(&mut self, opcode: u8, operand: u16) -> bool {
        if !self.can_route_exception() {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which `compile_pass`
        // published before dispatching and which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };

        // What the helper is, and what its SECOND argument is. The third is
        // either an immediate (the field count) or the length operand.
        let site = self.new_sites.get(&self.cur_bytecode_pc).copied();
        let (helper, class_or_atype) = match opcode {
            0xbb => match site {
                Some(s) => (self.helpers.new_object, i64::from(s.class_id)),
                None => return false,
            },
            // `atype` is an immediate operand byte, so there is nothing to
            // resolve and nothing that can be missing.
            0xbc => (self.helpers.newarray, i64::from(operand)),
            0xbd => match site {
                Some(s) => (self.helpers.anewarray_object, i64::from(s.class_id)),
                None => return false,
            },
            _ => return false,
        };
        if helper == 0 {
            return false;
        }

        // The length operand, for the two array forms. Popped BEFORE the
        // allocations below, so the allocator can see the register it frees.
        let length = match opcode {
            0xbc | 0xbd => {
                let reg = self.pop_kind(OperandKind::I32);
                if self.failed {
                    return false;
                }
                Some(reg)
            }
            _ => None,
        };
        let third = match (opcode, length) {
            (0xbb, _) => {
                let Some(s) = site else { return false };
                let Ok(fields) = i64::try_from(s.num_fields) else {
                    return false;
                };
                Arm64HelperArg::Imm(fields)
            }
            (_, Some(reg)) => Arm64HelperArg::Reg(reg),
            // Unreachable: the match above gave every non-`new` opcode a
            // length. Refuse rather than fabricate an argument.
            _ => return false,
        };

        // EVERY allocation before the branch. See `emit_helper_call`.
        let ctx = self.alloc_reg(false);
        let obj = self.alloc_reg(false);
        if self.failed {
            return false;
        }
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        if !self.emit_helper_call_at_safepoint(
            helper,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Imm(class_or_atype),
                third,
            ],
            Some(obj),
            safepoint_id,
        ) {
            return false;
        }

        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::Cbnz {
            rt: obj,
            label: join,
        });
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);

        self.push_reg(OperandKind::Ref, obj);
        // A fresh object IS a reference, and the oop marks are what every map
        // recorded after this point names its slot from.
        self.mark_top_operand_as_oop();
        true
    }

    // -- Invoke -------------------------------------------------------------

    /// Every `invoke*` form (`0xb6..=0xba`), round 9 wave 22 -- the first
    /// bytecode CALL this backend has ever compiled.
    ///
    /// ```text
    ///   LDR   Xctx,  [FP, #context]
    ///   SUB   Xargs, FP, #|offset of the deepest argument's depth slot|
    ///   <call_at_safepoint(jit_invoke_dispatch,
    ///                      [ctx, &JitInvokeInfo, args, n]) -> Xv,
    ///    consuming the n argument entries>
    ///   <bail if Xv is the sentinel; for J/D/F ask dispatch_threw first>
    ///   <push Xv as the descriptor's return type, marked if a reference>
    /// ```
    ///
    /// # Why this needs no call-target resolution
    ///
    /// `emit_invoke` set `self.failed` unconditionally from this backend's
    /// first commit, and the reason given -- and carried by three successive
    /// known-issue pages -- was that there is no way here to turn a
    /// constant-pool index into an entry point. That is still true, and it is
    /// no longer the question. `jit_invoke_dispatch` is the interpreter's own
    /// dispatcher: it takes the site's `JitInvokeInfo` and RESOLVES the target
    /// itself, at run time, through the same constant-pool machinery the
    /// interpreter uses -- virtual and interface selection on the receiver,
    /// JVMS §6.5's `invokespecial` redirect, a compiled callee's entry when
    /// one exists and the interpreter when it does not. So the four pieces the
    /// page split this into (a direct call, an inline cache, an outgoing Java
    /// argument ABI, a self-recursion guard) are all on the far side of ONE
    /// helper call, and this backend has had one of those since wave 18.
    ///
    /// What is given up is the speed of a direct call and of an inline cache.
    /// That is the right trade for a backend whose current alternative is not
    /// compiling the method at all.
    ///
    /// # The argument buffer is the operand area
    ///
    /// The helper reads `args_ptr as *const i64` with `args[0]` at the LOWEST
    /// address, one word per operand-stack VALUE (this VM's compact
    /// convention: a `long` is one). This backend's operand area is a run of
    /// frame words indexed by depth, `Arm64SpillArea::operand_word(depth) =
    /// locals + depth`, and `spill_word_offset` scales word indices UPWARD in
    /// address from `spill_offset`. So depth `base` -- the deepest argument,
    /// which is `args[0]` -- already sits at the lowest address of a
    /// contiguous ascending run of exactly the right words. The buffer needs
    /// no copy and no separate region: it is `&[FP + offset_of(base)]`, and
    /// the spill the call performs anyway is what fills it.
    ///
    /// That rests on an invariant worth naming, because it is what would break
    /// first: **an operand in a SLOT is always in ITS OWN depth's slot.** The
    /// only two places that ever set `OperandLoc::Slot` --
    /// [`Self::spill_entry`] and [`Self::restore_stack_at`] -- both address it
    /// through `spill_offset_for_depth(depth)`, and every stack shuffle here
    /// goes through `pop_entry`/`push_like`, which materialize into a REGISTER
    /// rather than renaming a slot. So an argument that was spilled long
    /// before this bytecode is already in the right word, and the call's own
    /// spill writes the rest to theirs.
    ///
    /// That is also why the oop story here is BETTER than x64's, which is
    /// worth recording because x64's is a known cost. There, an outgoing
    /// reference argument is marshalled into a staging area no frame-slot map
    /// can name, so every direct call taking a reference raises
    /// `pending_staged_args_unmapped` and the method loses `fully_oop_covered`
    /// for good (`moving_young_osr_method_needs_fallback` measured 439 of 449
    /// coverage failures on `TestKillProcessWhileWriting` as that one shape).
    /// Here the arguments never leave the operand stack until after the map is
    /// recorded, so each reference among them is named where it lies.
    ///
    /// # Refusals
    ///
    /// * An unresolved site -- no `JitInvokeInfo` for this pc.
    /// * An unwired `jit_invoke_dispatch`, or an unwired `dispatch_threw` at a
    ///   `J`/`D`/`F`-returning site.
    /// * `set_throw_bci` neither wired nor unneeded -- see
    ///   [`Self::can_route_exception`]. `synchronized` is still kept out, just
    ///   no longer by this check: its generated `any -> monitorexit; athrow`
    ///   handler makes the exception table non-empty, same as any other
    ///   `try`/`catch`, but `monitorenter`/`monitorexit` have no arm at all,
    ///   so that method refuses there instead, same as before this existed.
    /// * Fewer operand entries than the site consumes, which is a malformed
    ///   method rather than a shape.
    pub fn emit_invoke(&mut self, opcode: u8) -> bool {
        // `invokedynamic` is NOT one of the five in any useful sense.
        // `JitInvokeInfo::invoke_kind` encodes virtual/special/interface/
        // static and has no value for a call site that is not a class at all,
        // so `jit_invoke_dispatch` could not resolve one; and a method holding
        // an `invokedynamic` cannot stay compiled on the x64 tier either.
        // Refused HERE as well as by the caller declining to resolve the site,
        // because "the site table happened to be empty" is a different fact
        // from "this opcode has no lowering".
        if opcode == 0xba {
            return false;
        }
        if !self.can_route_exception() || self.helpers.invoke_dispatch == 0 {
            return false;
        }
        let Some(site) = self.invoke_sites.get(&self.cur_bytecode_pc).copied() else {
            return false;
        };
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };
        let Some(surviving) = self.operand_stack.len().checked_sub(site.num_jit_args) else {
            return false;
        };
        // `Long.MIN_VALUE` and a `double` of `-0.0` are bit-identical to the
        // deopt sentinel, and so is a `float` the helper sign-extends, so
        // those three returns have to ASK whether a throw is pending instead
        // of reading the bits -- exactly as `getfield`'s `J`/`D` arms do, and
        // as x64's post-invoke check does for the same three tags.
        let ambiguous = matches!(site.return_type, b'J' | b'D' | b'F');
        if ambiguous && self.helpers.dispatch_threw == 0 {
            return false;
        }

        // EVERY allocation before the branch. See `emit_helper_call`: the
        // sentinel test puts a call on one side of a join, and a register
        // allocated there would exist on one path only.
        let ctx = self.alloc_reg(false);
        let args_ptr = self.alloc_reg(false);
        let value = self.alloc_reg(false);
        let sentinel = self.alloc_reg(false);
        let probe = ambiguous.then(|| self.alloc_reg(false));
        if self.failed {
            return false;
        }

        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        // The buffer address. A zero-argument site still passes a pointer the
        // helper will not read -- it checks `num_args > 0` first -- and FP is
        // a real mapped address, which a literal 0 is not.
        if site.num_jit_args == 0 {
            self.buffer.emit(Arm64Instruction::Mov {
                rd: args_ptr,
                rm: Arm64Register::FP,
            });
        } else {
            let Some(off) = self.spill_offset_for_depth(surviving) else {
                self.failed = true;
                return false;
            };
            // Frame offsets are negative from FP; the buffer grows upward in
            // address from this one word.
            let Some(magnitude) = off.checked_neg() else {
                self.failed = true;
                return false;
            };
            self.buffer.emit(Arm64Instruction::SubImm {
                rd: args_ptr,
                rn: Arm64Register::FP,
                imm: magnitude,
            });
        }

        // Cast: an address the caller keeps alive with the artifact, and a
        // count that came from a descriptor.
        let info = site.info_ptr as i64;
        let Ok(num_args) = i64::try_from(site.num_jit_args) else {
            self.failed = true;
            return false;
        };
        // The first use of x64's ABI assertion on this backend. Worth having
        // exactly here: `emit_helper_call*` marshals `args.len()` registers,
        // so an argument count is DATA at each site rather than a shape the
        // emitter enforces, and a helper that grew a parameter would be a
        // silent ABI error instead of a build failure.
        cratonvm_jit_api::assert_helper_call_shape!(
            "invoke_dispatch",
            int_args = 4,
            returns_value = true
        );
        if !self.emit_helper_call_at_safepoint_consuming(
            self.helpers.invoke_dispatch,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Imm(info),
                Arm64HelperArg::Reg(args_ptr),
                Arm64HelperArg::Imm(num_args),
            ],
            Some(value),
            safepoint_id,
            site.num_jit_args,
        ) {
            return false;
        }

        // The sentinel edge, shaped exactly like `emit_getfield`'s.
        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: sentinel,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: value,
            rm: sentinel,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Ne,
            label: join,
        });
        if let Some(probe) = probe {
            let leave = self.buffer.new_label();
            if !self.emit_helper_call(self.helpers.dispatch_threw, &[], Some(probe)) {
                return false;
            }
            self.buffer.emit(Arm64Instruction::Cbnz {
                rt: probe,
                label: leave,
            });
            // No signal: the `i64::MIN` is the callee's real result.
            // Re-materialized because the probe call destroyed `value`, and on
            // this branch it can only have held the sentinel.
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: value,
                imm: i64::MIN,
            });
            self.buffer.emit(Arm64Instruction::B { label: join });
            self.buffer.bind_label(leave);
        }
        self.emit_stamp_throw_bci(self.cur_bytecode_pc);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.buffer.bind_label(join);

        // The result, decoded from X0 the way the VM decodes every compiled
        // return: a `float` is the low 32 bits of the word, a `double` all 64.
        match site.return_type {
            b'V' => {}
            b'I' | b'Z' | b'B' | b'C' | b'S' => {
                // The helper hands back a widened `int`; the canonical form on
                // this backend is sign-extended, and stating it here is what
                // keeps `iadd`'s 32-bit wrap correct on the result.
                self.emit_sxtw(value);
                self.push_reg(OperandKind::I32, value);
            }
            b'J' => self.push_reg(OperandKind::I64, value),
            b'F' => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFpSingle { vd: v, rn: value });
                self.push_reg(OperandKind::F32, v);
            }
            b'D' => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFp { vd: v, rn: value });
                self.push_reg(OperandKind::F64, v);
            }
            b'L' | b'[' => {
                self.push_reg(OperandKind::Ref, value);
                self.mark_top_operand_as_oop();
            }
            // A return tag no descriptor produces: refuse rather than guess a
            // width for it.
            _ => {
                self.failed = true;
                return false;
            }
        }
        !self.failed
    }

    /// `checkcast` (`0xc0`) and `instanceof` (`0xc1`), round 9 wave 23.
    ///
    /// ```text
    ///   LDR   Xctx, [FP, #context]
    ///   <call_at_safepoint(jit_checkcast | jit_instanceof_check,
    ///                      [ctx, obj, name_ptr, name_len]) -> Xv,
    ///    consuming the object entry>
    ///   <bail if Xv is the sentinel: a ClassCastException is pending>
    ///   <push Xv: the reference for 0xc0, an int for 0xc1>
    /// ```
    ///
    /// The object is NOT popped before the call, for the reason `aastore`
    /// established: an entry the model still holds is spilled to its depth
    /// slot and named in the map, so a collector that moves it during the
    /// helper -- which is a real possibility here, because resolving the
    /// target class on first use allocates its `java/lang/Class` mirror --
    /// rewrites the word the helper reads. `checkcast`'s answer is then the
    /// object's NEW address, because the helper returns the reference it
    /// resolved rather than the bits it was handed.
    ///
    /// x64 has inline fast paths in front of this call (a trusted-oop check, a
    /// `KIND_TAGS == 0` screen for arrays, a class-id compare). None is
    /// emitted here: they need the receiver's class id from the header, which
    /// this backend has no reason to read yet, and the point of this wave is
    /// that the opcodes COMPILE.
    ///
    /// Refuses without a resolved site, an unwired helper, a context word, or
    /// `set_throw_bci` neither wired nor unneeded (see
    /// [`Self::can_route_exception`]) -- a failed cast returns the sentinel
    /// with a `ClassCastException` stashed, and [`Self::emit_bail_on_sentinel`]
    /// stamps the throw bci on that edge so the drain routes it through this
    /// method's own exception table.
    pub(in crate::aarch64_backend) fn emit_typecheck(&mut self, opcode: u8) -> bool {
        let helper = if opcode == 0xc0 {
            self.helpers.checkcast
        } else {
            self.helpers.instanceof_check
        };
        if helper == 0 || !self.can_route_exception() {
            return false;
        }
        let Some(site) = self.typecheck_sites.get(&self.cur_bytecode_pc).copied() else {
            return false;
        };
        // An unresolved target makes the helper answer "not an instance" for
        // every object, which is a WRONG answer rather than a missing one.
        let Ok(name_len) = i64::try_from(site.name_len) else {
            return false;
        };
        if site.name_ptr == 0 || name_len == 0 {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        // Cast: the bci of the site being lowered, which keys the oop map.
        let Ok(safepoint_id) = u32::try_from(self.cur_bytecode_pc) else {
            return false;
        };

        // Left on the stack, and held. See the doc.
        let Some((obj, entry)) = self.materialize_entry(0) else {
            return false;
        };
        if entry.kind != OperandKind::Ref {
            self.failed = true;
            return false;
        }

        // EVERY allocation before the branch. See `emit_helper_call`.
        let ctx = self.alloc_reg(false);
        let value = self.alloc_reg(false);
        let sentinel = self.alloc_reg(false);
        if self.failed {
            return false;
        }

        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ctx,
            rn: Arm64Register::FP,
            offset: ctx_off,
        });
        // Cast: an interned address that outlives every artifact.
        let name_ptr = site.name_ptr as i64;
        // Both helpers have the same shape, and the `class_name_ptr` counts as
        // one integer argument -- it is loaded as a full 64-bit immediate,
        // exactly as x64's two sites load it.
        cratonvm_jit_api::assert_helper_call_shape!(
            "checkcast",
            int_args = 4,
            returns_value = true
        );
        cratonvm_jit_api::assert_helper_call_shape!(
            "instanceof_check",
            int_args = 4,
            returns_value = true
        );
        if !self.emit_helper_call_at_safepoint_consuming(
            helper,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Reg(obj),
                Arm64HelperArg::Imm(name_ptr),
                Arm64HelperArg::Imm(name_len),
            ],
            Some(value),
            safepoint_id,
            1,
        ) {
            return false;
        }
        // `i64::MIN` is neither a reference (a heap pointer is under 2^48) nor
        // a `0`/`1`, so it is unambiguous for both opcodes.
        self.emit_bail_on_sentinel(value, sentinel);
        if opcode == 0xc0 {
            self.push_reg(OperandKind::Ref, value);
            self.mark_top_operand_as_oop();
        } else {
            self.emit_sxtw(value);
            self.push_reg(OperandKind::I32, value);
        }
        !self.failed
    }

    // -- Return -------------------------------------------------------------

    /// `ireturn`/`lreturn`/`areturn`/`freturn`/`dreturn`.
    ///
    /// The VM calls compiled code as `extern "C" fn(i64, ..) -> i64` and reads
    /// EVERY result out of X0, decoding a `float` as `f32::from_bits(x0 as
    /// u32)` and a `double` as `f64::from_bits(x0)`. So an FP result is moved
    /// into X0 with a bit-exact `FMOV` (`FMOV W0, Sn` for a `float`), not left
    /// in V0. `freturn`/`dreturn` used to pop and DISCARD the value and emit a
    /// bare `RET`, skipping the epilogue: the caller's callee-saved registers
    /// were never restored, SP and FP were left pointing into this frame, and
    /// the "result" was whatever X0 held.
    pub(in crate::aarch64_backend) fn emit_return_value(&mut self, kind: OperandKind) {
        let src = self.pop_kind(kind);
        let inst = match kind {
            OperandKind::F32 => Arm64Instruction::FmovFromFpSingle {
                rd: Arm64Register::X0,
                vn: src,
            },
            OperandKind::F64 => Arm64Instruction::FmovFromFp {
                rd: Arm64Register::X0,
                vn: src,
            },
            _ => Arm64Instruction::Mov {
                rd: Arm64Register::X0,
                rm: src,
            },
        };
        self.buffer.emit(inst);
        if kind == OperandKind::I32 {
            self.emit_narrow_int_return();
        }
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.reachable = false;    }

    /// JVMS §6.5 `ireturn`: narrow X0 to the declared `boolean`/`byte`/
    /// `char`/`short` return type, keeping this backend's `int` convention
    /// (sign-extended through all 64 bits): `Z` is `& 1` and `C` is `& 0xFFFF`
    /// (never negative, so zero-extension IS the sign extension), `B`/`S` are
    /// the X-form `SXTB`/`SXTH`. The x64 twin is `emit_narrow_int_return` in
    /// `x64/emit.rs`; before this the two backends disagreed on what a
    /// `boolean` method returning `2` hands back.
    pub(in crate::aarch64_backend) fn emit_narrow_int_return(&mut self) {
        let x0 = Arm64Register::X0;
        let inst = match self.return_narrow {
            Some(b'Z') => Arm64Instruction::AndImm {
                rd: x0,
                rn: x0,
                imm: 1,
            },
            Some(b'B') => Arm64Instruction::Sxtb { rd: x0, rn: x0 },
            Some(b'C') => Arm64Instruction::AndImm {
                rd: x0,
                rn: x0,
                imm: 0xFFFF,
            },
            Some(b'S') => Arm64Instruction::Sxth { rd: x0, rn: x0 },
            _ => return,
        };
        self.buffer.emit(inst);
    }
}
// NOTE: A `detect_neon_patterns` / `NeonVectorizablePattern` bytecode scanner
// previously lived here. It was dead code — only ever called from its own unit
// tests, never wired into any aarch64 codegen path — and, worse, it emitted
// patterns with hardcoded placeholder local indices (`array_local: 0` etc.) with
// the operand analysis left unfinished, so it would have vectorized against the
// wrong locals (a miscompile) if ever consumed. aarch64 is not the production
// backend (x64.rs is). Removed in the 2026-06-10 JIT cleanup pass rather than
// gated, since nothing non-test referenced it. If NEON auto-vectorization is
// pursued, reuse the real operand resolution from the x64 BCE analysis
// (`analyze_array_access_operands` / `find_induction_variable`) instead of
// placeholders, and wire the result into codegen before adding it back.

// ---------------------------------------------------------------------------
// Machine code emission
// ---------------------------------------------------------------------------

// `to_reg` used to live here: `Arm64Register -> Option<Reg>` over 0..=31,
// whose one production caller was `r`. After A8 `r` builds a `RegSp` directly
// (31 is SP there, and `Reg` no longer has a 31 to give), so `to_reg` had no
// production caller left and was removed rather than kept as a second spelling
// of `Reg::from_u8`. Its C11 contract -- refuse by returning `None`, never
// assert, so a debug build declines a method exactly where a release build
// does -- is carried by the three conversions below and by `Reg::from_u8`.

/// Convert an `Arm64Register` to an `FpReg` for the emitter.
///
/// The FP half of the `Arm64Register` space is `32 + n` for `Dn`, and `FpReg`
/// models the full architectural file `D0..D31`, so the accepted range is
/// `32..=63`. This used to say "V0=32..V7=39", which is the register
/// ALLOCATOR's pool (`ARM64_LOCAL_FPS`) rather than this conversion's range —
/// a test was written against that sentence and asserted that `D8` was
/// unencodable.
///
/// Returns `None` if `r` is not a valid FP register encoding. See [`to_reg`]
/// for why this returns `Option` and why it does not assert.
pub(in crate::aarch64_backend) fn to_fpreg(r: Arm64Register) -> Option<crate::aarch64::FpReg> {
    if r.0 < 32 {
        return None;
    }
    crate::aarch64::FpReg::from_u8(r.0 - 32)
}

thread_local! {
    /// Sticky "an `Arm64Register` was not a valid encoding" flag for the
    /// current [`emit_machine_code_inner`] call.
    ///
    /// A5. `r` and `fp` used to `.expect()`, i.e. **panic**, on a register the
    /// allocator should never have produced. Every other refusal in this file
    /// sets `self.failed` or returns `None` from `emit_machine_code`, and the
    /// crate's rule is that a compiler bug costs the method its compiled body,
    /// not the process. This is not hypothetical for the FP side: `D8`-`D15`
    /// once arrived as `Arm64Register(40..=47)` and tripped the `debug_assert`
    /// that the `expect` replaced (aarch64 parity audit §4.3).
    ///
    /// Modelled on `Aarch64Emitter::overflowed()` -- the same "note it, keep
    /// encoding, discard the buffer at the end" shape, which is what lets the
    /// two `r`/`fp` helpers keep their infallible signature at several hundred
    /// call sites. Thread-local rather than global because one method is
    /// encoded on one thread, so two concurrent compiles cannot poison each
    /// other's result.
    pub(in crate::aarch64_backend) static INVALID_REGISTER_SEEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Record that a register could not be encoded.
pub(in crate::aarch64_backend) fn note_invalid_register() {
    INVALID_REGISTER_SEEN.with(|seen| seen.set(true));
}

/// Read and clear the sticky invalid-register flag.
pub(in crate::aarch64_backend) fn take_invalid_register() -> bool {
    INVALID_REGISTER_SEEN.with(|seen| seen.replace(false))
}

/// Shorthand for `to_reg` at call sites where the register allocator
/// guarantees the register is a valid GPR. Centralising it keeps the regalloc
/// contract documented in one place.
///
/// A violated invariant is recorded in [`INVALID_REGISTER_SEEN`] and
/// `emit_machine_code_inner` then discards the whole body, so the register
/// returned here is a placeholder whose bytes are never published and its
/// identity does not matter. IP0 is used because it is the one register no
/// lowering treats as carrying a live value across an instruction boundary.
///
/// # A8: this is the SP-capable conversion, and says so in its type
///
/// It answers [`crate::aarch64::RegSp`], reading encoding 31 as the stack
/// pointer, because the fields it feeds -- a load/store base, an add/sub
/// immediate `rd`/`rn`, an extended-register `rd`/`rn` -- are the ones that read
/// 31 as SP: `SUB SP, SP, #imm` and `STP FP, LR, [SP, #-16]!` are real
/// instructions the prologue emits through here.
///
/// Before A8 it answered a plain `Reg`, and `Reg` contained `SP`. Removing `SP`
/// from `Reg` without changing this function would have COMPILED -- `to_reg`
/// would simply have started answering `None` for 31 -- and silently turned
/// every prologue into a refused compile that also encoded X16 where it meant
/// SP. That is the "half-finished split is silently wrong" failure A8's own
/// note warned of, caught by reading rather than by the compiler. The return
/// type is what now makes it a compiler error: a `RegSp` cannot reach a field
/// that reads 31 as XZR, so every GP-only operand has to go through [`r_gp`].
#[inline]
pub(in crate::aarch64_backend) fn r(reg: Arm64Register) -> crate::aarch64::RegSp {
    match crate::aarch64::RegSp::from_u8(reg.0) {
        Some(reg) => reg,
        None => {
            note_invalid_register();
            crate::aarch64::RegSp::X(crate::aarch64::Reg::X16)
        }
    }
}

/// [`r`] for an operand field of a **shifted-register** instruction, where
/// encoding 31 is XZR and not SP.
///
/// # A8: the other place provenance is visible
///
/// Before A8, `XZR` and `Reg::SP` were one value, so no check on an operand
/// could say which one a caller meant -- see `aarch64::XZR`. Since A8 the TYPE
/// says it (this yields a GP-only `Reg`, [`r`] a `RegSp`, `r_zr` a `RegZr`), and
/// what follows is why this conversion also REFUSES 31. The arm of
/// `emit_machine_code_inner`'s match that is about to emit an `ADD Xd, Xn, Xm`
/// knows something the operand does not: it knows the ENCODING. In a
/// shifted-register form every register field reads 31 as XZR, so an
/// `Arm64Register` carrying 31 into one of those fields is never what the
/// caller meant — a real zero operand is written as the literal
/// `crate::aarch64::XZR` at the call site (the `Neg` arm does exactly that),
/// and a literal is not the thing that gets forwarded by accident.
///
/// So: this refuses 31 and [`r`] does not. `r` stays for the addsub-immediate,
/// extended-register and load/store arms, where 31 legitimately means SP — the
/// prologue's `SUB SP, SP, #imm` and `STP FP, LR, [SP, #-16]!` are that shape.
///
/// A refusal records through [`note_invalid_register`] and costs the method its
/// compiled body, which is this file's standard answer to a compiler bug (A5)
/// and is the safe direction: the alternative was `ADD X16, SP, X16` computing
/// `0 + X16` and sending an access to an absolute address.
///
/// **If this ever refuses something legitimate**, the fix is at the call site,
/// not here: write `crate::aarch64::XZR` instead of routing a 31 through an
/// `Arm64Register`.
#[inline]
pub(in crate::aarch64_backend) fn r_gp(reg: Arm64Register) -> crate::aarch64::Reg {
    match crate::aarch64::Reg::from_u8_gp(reg.0) {
        Some(reg) => reg,
        None => {
            note_invalid_register();
            crate::aarch64::Reg::X16
        }
    }
}

/// [`r`] for a field that reads encoding 31 as the ZERO register and may
/// legitimately be given it.
///
/// # A8: the third conversion, and why it had to exist
///
/// After the split this file has three ways to turn an `Arm64Register` into an
/// operand, chosen by what the FIELD does with 31 -- never by the operand:
///
/// | conversion | yields | 31 becomes | for |
/// |---|---|---|---|
/// | [`r`] | `RegSp` | SP | load/store bases; add/sub-immediate and extended `rd`/`rn` |
/// | `r_zr` | `RegZr` | XZR | data fields that may be zero: a load/store `Rt`, `CBZ`'s register, the GP side of `scvtf`/`fcvtzs`/`fmov`, a branch target |
/// | [`r_gp`] | `Reg` | refused | fields where 31 is always a bug |
///
/// This one was NOT in the first draft of the split, and the omission was a
/// real regression caught by `a_large_frame_bangs_every_page_before_moving_sp`.
/// The first draft sent every non-SP field to `r_gp`, reasoning that a GP field
/// given 31 was a bug. But the stack-bang probe stores ZERO with
/// `STR XZR, [X16]` -- its `Rt` is 31 on purpose -- and `r_gp` refused it, so
/// every large-frame prologue stopped encoding. More generally, before A8 every
/// one of these fields accepted 31 and encoded it as 31, because `r` answered
/// `Reg::SP`; `r_zr` keeps exactly those bits, where `r_gp` would have turned
/// each into a refusal.
#[inline]
pub(in crate::aarch64_backend) fn r_zr(reg: Arm64Register) -> crate::aarch64::RegZr {
    if reg.0 == 31 {
        return crate::aarch64::RegZr::Zr;
    }
    match crate::aarch64::Reg::from_u8(reg.0) {
        Some(reg) => crate::aarch64::RegZr::X(reg),
        None => {
            note_invalid_register();
            crate::aarch64::RegZr::X(crate::aarch64::Reg::X16)
        }
    }
}

/// Shorthand for `to_fpreg` at call sites where the register allocator
/// guarantees the register is a valid FP register. See [`r`].
#[inline]
pub(in crate::aarch64_backend) fn fp(reg: Arm64Register) -> crate::aarch64::FpReg {
    match to_fpreg(reg) {
        Some(reg) => reg,
        None => {
            note_invalid_register();
            crate::aarch64::FpReg::D0
        }
    }
}

/// Convert an `Arm64Condition` to an `aarch64::Cond`.
pub(in crate::aarch64_backend) fn to_cond(c: &Arm64Condition) -> crate::aarch64::Cond {
    match c {
        Arm64Condition::Eq => crate::aarch64::Cond::EQ,
        Arm64Condition::Ne => crate::aarch64::Cond::NE,
        Arm64Condition::Lt => crate::aarch64::Cond::LT,
        Arm64Condition::Le => crate::aarch64::Cond::LE,
        Arm64Condition::Gt => crate::aarch64::Cond::GT,
        Arm64Condition::Ge => crate::aarch64::Cond::GE,
        Arm64Condition::Hi => crate::aarch64::Cond::HI,
        Arm64Condition::Ls => crate::aarch64::Cond::LS,
        Arm64Condition::Cs => crate::aarch64::Cond::HS,
        Arm64Condition::Cc => crate::aarch64::Cond::LO,
        Arm64Condition::Mi => crate::aarch64::Cond::MI,
        Arm64Condition::Pl => crate::aarch64::Cond::PL,
        Arm64Condition::Vs => crate::aarch64::Cond::VS,
        Arm64Condition::Vc => crate::aarch64::Cond::VC,
        Arm64Condition::Al => crate::aarch64::Cond::AL,
    }
}

/// Materialize the effective address `base + offset` into IP0 (X16).
///
/// Bug-fix (ARM64 BUG #1): used by the `Ldr`/`Str` lowering when `offset`
/// falls outside the 9-bit signed range that LDUR/STUR can encode. We load the
/// (possibly large, possibly negative) byte offset into IP0 with `mov_imm64`
/// (sign-extended via the MOVN path for negatives) and add the base, so the
/// subsequent zero-offset access never touches the base register's value. IP0
/// (X16) is the AAPCS64 intra-procedure-call scratch register and is never a
/// regalloc output, so clobbering it here is safe.
///
/// Returns `false`, emitting nothing useful, when `base` is ITSELF IP0. The
/// sequence writes the offset into X16 *before* the ADD reads the base, so an
/// X16 base is destroyed before it is used and the resulting address is
/// `offset + offset`. The claim that "IP0 is never a regalloc output" is true
/// of the allocator and not of this backend's own X16 uses -- the stack-bang
/// probe, the `tableswitch` scratch and the poll/frame-record address
/// materialisation all put things in X16 -- so the caller must abandon the
/// method rather than assume the shape cannot arise. `emit_addsub_imm_safe`
/// and the `AndImm` lowering already guard the same hazard; this closes the
/// load/store arms.
#[must_use]
pub(in crate::aarch64_backend) fn emit_addr_into_ip0(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    base: impl Into<crate::aarch64::RegSp>,
    offset: i32,
) -> bool {
    let base: crate::aarch64::RegSp = base.into();
    if base.enc() == 16 {
        return false;
    }
    // IP0 = (i64)offset  (mov_imm64 picks MOVN for negative values, giving a
    // correct two's-complement 64-bit value).
    emitter.mov_imm64(crate::aarch64::Reg::X16, offset as i64 as u64);
    // IP0 = base + IP0, in the EXTENDED-register form. The shifted-register
    // ADD reads register 31 as XZR, so with an SP base it computed `0 + offset`
    // and the access went to an absolute address. The extended form reads 31
    // as SP and is otherwise the same instruction.
    emitter.add_ext(
        crate::aarch64::Reg::X16,
        base,
        crate::aarch64::Reg::X16,
        crate::aarch64::Extend::UXTX,
        0,
    );
    true
}

/// Lower an ADD/SUB-immediate (`rd = rn ± imm`) safely.
///
/// Bug-fix (AArch64 LOW, immediate truncation): the ADD/SUB-immediate
/// encoding (`addsub_imm`) only carries a 12-bit unsigned field and silently
/// masks anything wider (`imm12 as u32 & 0xFFF`). The previous lowering passed
/// `imm as u16` straight through, so any immediate > 0xFFF (e.g. a large
/// `iinc` constant or a frame offset folded into an `AddImm`) was silently
/// truncated to a WRONG value with no diagnostic.
///
/// This helper picks the correct, lossless encoding:
///   * magnitude fits 12 bits (≤ 0xFFF)                  → ADD/SUB #imm12
///   * magnitude fits the LSL-#12 shifted 12-bit form
///     (low 12 bits zero, high 12 bits ≤ 0xFFF)          → ADD/SUB #imm12, LSL #12
///   * otherwise                                         → materialize the
///     immediate into IP0 (X16) with `mov_imm64` and use the register form
///     ADD/SUB `rd, rn, X16`.
///
/// The sign is normalized first: a negative immediate flips ADD↔SUB so the
/// magnitude handed to the encoding is always non-negative. IP0 (X16) is the
/// AAPCS64 intra-procedure-call scratch and is never a regalloc output, so the
/// register-form fallback never clobbers a live value.
///
/// The register fallback uses the EXTENDED-register form, so it is sound with
/// SP as `rd` or `rn` too. It used to be the shifted-register form, where 31
/// is XZR, so an SP adjustment wider than 12 bits had no lowering at all -- it
/// first emitted `BRK #0` while reporting success, later refused the method,
/// and either way a frame of 4096 bytes or more could not be allocated.
///
/// Returns `false` only when the fallback would clobber its own operand (`rn`
/// is IP0), in which case the caller must abandon the method.
#[must_use]
pub(in crate::aarch64_backend) fn emit_addsub_imm_safe(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    rd: impl Into<crate::aarch64::RegSp>,
    rn: impl Into<crate::aarch64::RegSp>,
    imm: i32,
    is_sub: bool,
) -> bool {
    let (rd, rn): (crate::aarch64::RegSp, crate::aarch64::RegSp) = (rd.into(), rn.into());
    // Normalize: fold the sign into the operation so `mag` is non-negative.
    // (i32::MIN's magnitude does not fit i32, so widen to i64 first.)
    let signed = if is_sub { -(imm as i64) } else { imm as i64 };
    let effective_sub = signed < 0;
    let mag = signed.unsigned_abs();

    if mag <= 0xFFF {
        // Fits the plain 12-bit immediate form.
        if effective_sub {
            emitter.sub_imm(rd, rn, mag as u16, false);
        } else {
            emitter.add_imm(rd, rn, mag as u16, false);
        }
        true
    } else if mag & 0xFFF == 0 && (mag >> 12) <= 0xFFF {
        // Fits the LSL #12 shifted 12-bit immediate form.
        let hi = (mag >> 12) as u16;
        if effective_sub {
            emitter.sub_imm(rd, rn, hi, true);
        } else {
            emitter.add_imm(rd, rn, hi, true);
        }
        true
    } else if rn.enc() == 16 {
        // The fallback materializes into IP0, which would destroy an operand
        // that is itself IP0 before it is read. No lowering emits that shape.
        false
    } else {
        // Out of immediate range: materialize into IP0 (X16) and use the
        // extended-register form, which reads 31 as SP on both sides. Never
        // silently truncate.
        emitter.mov_imm64(crate::aarch64::Reg::X16, mag);
        if effective_sub {
            emitter.sub_ext(
                rd,
                rn,
                crate::aarch64::Reg::X16,
                crate::aarch64::Extend::UXTX,
                0,
            );
        } else {
            emitter.add_ext(
                rd,
                rn,
                crate::aarch64::Reg::X16,
                crate::aarch64::Extend::UXTX,
                0,
            );
        }
        true
    }
}

/// Lower `rd = rn +/- imm` on W registers (`AddImmW`/`SubImmW`).
///
/// A 32-bit result depends only on the addend mod 2^32, so a wide immediate is
/// materialized as its low 32 bits. `false` when that would clobber `rn`.
#[must_use]
pub(in crate::aarch64_backend) fn emit_addsub_imm_w(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    rd: crate::aarch64::Reg,
    rn: crate::aarch64::Reg,
    imm: i32,
    is_sub: bool,
) -> bool {
    let signed = if is_sub {
        -i64::from(imm)
    } else {
        i64::from(imm)
    };
    let mag = signed.unsigned_abs();
    if mag <= 0xFFF {
        // Cast: bounds-checked immediately above.
        if signed < 0 {
            emitter.sub_imm_w(rd, rn, mag as u16, false);        } else {
            emitter.add_imm_w(rd, rn, mag as u16, false);        }
        true    } else if rn.enc() == 16 {
        false
    } else {
        // Cast: the low 32 bits of the two's-complement addend.
        emitter.mov_imm64(crate::aarch64::Reg::X16, u64::from(signed as u32));
        emitter.add_w(rd, rn, crate::aarch64::Reg::X16);
        true
    }
}

/// `rd = rn - value` on W registers, through IP0 when `value` is wide.
pub(in crate::aarch64_backend) fn emit_w_sub_const(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    rd: crate::aarch64::Reg,
    rn: crate::aarch64::Reg,
    value: i32,
) {
    let v = i64::from(value);
    if (0..=0xFFF).contains(&v) {
        // Cast: range-checked.
        emitter.sub_imm_w(rd, rn, v as u16, false);
    } else if (-0xFFF..0).contains(&v) {
        // Cast: range-checked.
        emitter.add_imm_w(rd, rn, (-v) as u16, false);
    } else {
        // Cast: the constant's 32-bit pattern.
        emitter.mov_imm64(crate::aarch64::Reg::X16, u64::from(value as u32));
        emitter.sub_w(rd, rn, crate::aarch64::Reg::X16);
    }
}

/// Set the flags for `Wrn - value`: `CMP #imm`, `CMN #-imm`, or a compare
/// against IP0. Never a scratch register, so it cannot collide with `rn`.
pub(in crate::aarch64_backend) fn emit_w_cmp_const(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    rn: crate::aarch64::Reg,
    value: i32,
) {
    let v = i64::from(value);
    if (0..=0xFFF).contains(&v) {
        // Cast: range-checked.
        emitter.cmp_imm_w(rn, v as u16);
    } else if (-0xFFF..0).contains(&v) {
        // Cast: range-checked.
        emitter.cmn_imm_w(rn, (-v) as u16);
    } else {
        // Cast: the constant's 32-bit pattern.
        emitter.mov_imm64(crate::aarch64::Reg::X16, u64::from(value as u32));
        emitter.cmp_w(rn, crate::aarch64::Reg::X16);
    }
}

/// Emit machine code bytes from the ARM64 pseudo-instruction sequence.
/// Uses `Aarch64Emitter` from `aarch64.rs` to encode each instruction.
///
/// Returns `None` — bail this method to the interpreter — when the pseudo-op
/// sequence cannot be encoded soundly. Six independent reasons (0–5 below);
/// reasons 4 and 5 were added by the 2026-09 encoder audit, which found the
/// scaled load/store encoders masking an over-wide offset into a valid-looking
/// one and the two register shorthands panicking instead of refusing:
///
/// 0. an `AddImm`/`SubImm` with no sound encoding — see
///    [`emit_addsub_imm_safe`], which returns `false` for the SP case that
///    used to emit `BRK #0` while still reporting a successful compile;
/// 1. `result.success == false` (an opcode arm refused the method);
/// 2. a branch or literal reference whose label is never bound (see the patch
///    loops at the end — an unbound label used to be left as "branch to
///    self", i.e. an infinite loop in *successfully* compiled code);
/// 3. `Aarch64Emitter::overflowed()` — a branch displacement that did not fit
///    its encoding field. `aarch64.rs` sets that sticky flag precisely so a
///    release build (where the parallel `debug_assert!` is compiled out) can
///    discard the buffer instead of executing a truncated branch. Until this
///    check was added the flag had **no production reader** anywhere in the
///    crate, so on a release `aarch64` build an out-of-range branch was
///    silently truncated and emitted as executable code.
/// 4. an immediate no load/store encoding can carry exactly — a scaled
///    unsigned offset that is misaligned or over 12 bits, an `i16`-overflowing
///    pair offset, or a far `[base + offset]` whose base or stored value is
///    IP0, the very register the address materialisation destroys;
/// 5. a register that is not a valid encoding, noted by [`r`]/[`fp`] in
///    [`INVALID_REGISTER_SEEN`] and checked once at the end. Those two used to
///    `.expect()`, so a regalloc bug killed the process instead of costing one
///    method its compiled body.
/// Encode a compiled method, and report where every pseudo-op landed.
///
/// The second element is `pseudo_offsets`: `pseudo_offsets[i]` is the byte
/// offset at which `result.instructions[i]` was encoded, and the vector has one
/// extra trailing entry equal to the total code length, so a safepoint whose
/// following pseudo-op is one past the end still resolves. This is the mapping
/// an aarch64 oop map has to be keyed through -- see [`Arm64PendingOopMap`] for
/// why `instruction_count * 4` is not it.
pub(in crate::aarch64_backend) fn emit_machine_code_inner(result: &Arm64CompileResult) -> Option<(Vec<u8>, Vec<usize>)> {
    use crate::aarch64::Aarch64Emitter;

    // Start from a clean slate: an earlier call may have returned through one
    // of the `return None` paths below with the flag still set.
    let _ = take_invalid_register();

    if !result.success {
        return None;
    }

    let mut emitter = Aarch64Emitter::new();
    let mut label_offsets: HashMap<u32, usize> = HashMap::new();
    // (code_offset, label_id, is_cond) — is_cond distinguishes B from B.cond/CBZ/CBNZ
    let mut branch_patches: Vec<(usize, u32, bool)> = Vec::new();
    // (code_offset, label_id) for LDR literal instructions needing pool offset patching
    let mut literal_patches: Vec<(usize, u32)> = Vec::new();
    // (entry_offset, table_base, label_id) for jump-table words
    let mut table_patches: Vec<(usize, usize, u32)> = Vec::new();

    // One entry per pseudo-op, recorded BEFORE it is encoded, so
    // `pseudo_offsets[i]` is where instruction `i` begins.
    let mut pseudo_offsets: Vec<usize> = Vec::with_capacity(result.instructions.len() + 1);
    for inst in &result.instructions {
        pseudo_offsets.push(emitter.offset());
        match inst {
            Arm64Instruction::Label(id) => {
                label_offsets.insert(*id, emitter.offset());
            }
            Arm64Instruction::Comment(_) => { /* skip */ }
            Arm64Instruction::ConstantPoolEntry { label, value } => {
                // Bind the label to the current offset, then emit the raw 64-bit value.
                label_offsets.insert(*label, emitter.offset());
                emitter.emit_u64_data(*value);
            }

            // -- Arithmetic --
            Arm64Instruction::Add { rd, rn, rm } => {
                emitter.add(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::AddLsl { rd, rn, rm, shift } => {
                // The imm6 field holds 0..=63; the array widths need 0..=3 and
                // anything else is a lowering bug, not a wide immediate to
                // materialize, so refuse the method rather than truncate.
                if *shift > 3 {
                    return None;
                }
                emitter.add_lsl(r_gp(*rd), r_gp(*rn), r_gp(*rm), *shift);
            }
            Arm64Instruction::AddImm { rd, rn, imm } => {
                // Range-checked lowering: fits 12-bit / shifted-12-bit form, or
                // materializes into IP0 and uses the register form. Never
                // silently truncates a wide immediate (see emit_addsub_imm_safe).
                // `false` = no sound encoding exists → bail the method.
                if !emit_addsub_imm_safe(&mut emitter, r(*rd), r(*rn), *imm, false) {
                    return None;
                }
            }
            Arm64Instruction::Sub { rd, rn, rm } => {
                emitter.sub(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::SubImm { rd, rn, imm } => {
                if !emit_addsub_imm_safe(&mut emitter, r(*rd), r(*rn), *imm, true) {
                    return None;
                }
            }
            Arm64Instruction::Mul { rd, rn, rm } => {
                emitter.mul(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::SDiv { rd, rn, rm } => {
                emitter.sdiv(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::Neg { rd, rn } => {
                // NEG Xd, Xn = SUB Xd, XZR, Xn. In the shifted form this emits,
                // register 31 is the zero register. Before A8 `XZR` and
                // `Reg::SP` were the same value and writing `SP` here compiled;
                // now `sub` takes a `RegZr` and `SP` is a `RegSp`, so it cannot.
                emitter.sub(r_gp(*rd), crate::aarch64::XZR, r_gp(*rn));
            }
            Arm64Instruction::Madd { rd, rn, rm, ra } => {
                emitter.madd(r_gp(*rd), r_gp(*rn), r_gp(*rm), r_gp(*ra));
            }
            Arm64Instruction::Msub { rd, rn, rm, ra } => {
                emitter.msub(r_gp(*rd), r_gp(*rn), r_gp(*rm), r_gp(*ra));
            }

            // -- Logical --
            Arm64Instruction::And { rd, rn, rm } => {
                emitter.and(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::Orr { rd, rn, rm } => {
                emitter.orr(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::Eor { rd, rn, rm } => {
                emitter.eor(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::Lsl { rd, rn, rm } => {
                emitter.lsl(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::Lsr { rd, rn, rm } => {
                emitter.lsr(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::Asr { rd, rn, rm } => {
                emitter.asr(r_gp(*rd), r_gp(*rn), r_gp(*rm));
            }

            // -- Compare --
            Arm64Instruction::Cmp { rn, rm } => {
                emitter.cmp(r_gp(*rn), r_gp(*rm));
            }
            Arm64Instruction::CmpImm { rn, imm } => {
                // Range-checked lowering. CMP #imm12 (= SUBS XZR, rn, #imm12)
                // only carries an unsigned 12-bit field. A wider or negative
                // immediate cannot be encoded directly, so we materialize it
                // into IP0 (X16) and use the register form `CMP rn, X16`
                // (= SUBS XZR, rn, X16). Never silently truncate (the old
                // `*imm as u16` masked anything > 0xFFFF and the encoder then
                // masked again to 12 bits). The shifted LSL #12 form is left to
                // the register-materialization path to avoid threading a new
                // shifted-immediate encoder through aarch64.rs; CMP immediates
                // in this backend are small (almost always 0) so this costs at
                // most one extra MOV in a cold path.
                let imm = *imm;
                if imm >= 0 && imm <= 0xFFF {
                    emitter.cmp_imm(r(*rn), imm as u16);
                } else {
                    // Negative or out-of-range: materialize and compare by reg.
                    emitter.mov_imm64(crate::aarch64::Reg::X16, imm as i64 as u64);
                    emitter.cmp(r_gp(*rn), crate::aarch64::Reg::X16);
                }
            }
            Arm64Instruction::Tst { rn, rm } => {
                emitter.tst(r_gp(*rn), r_gp(*rm));
            }

            // -- Move --
            Arm64Instruction::Mov { rd, rm } => {
                emitter.mov(r_gp(*rd), r_gp(*rm));
            }
            Arm64Instruction::MovImm { rd, imm } => {
                emitter.mov_imm64(r_gp(*rd), *imm as u64);
            }
            Arm64Instruction::MovK { rd, imm, shift } => {
                emitter.movk(r_gp(*rd), *imm, *shift);
            }

            // -- Load / Store --
            //
            // Bug-fix (ARM64 BUG #1, frame-slot corruption): plain
            // `[base + #offset]` access must NEVER use the pre-index
            // writeback form (`ldr_pre`/`str_pre`), which mutates the base
            // register `base = base + offset` as a side effect. Every frame
            // slot is addressed at a NEGATIVE offset from FP, so the old code
            // corrupted FP on every spill/reload. Routing:
            //   * offset >= 0 and 8-aligned and in scaled range → scaled
            //     unsigned-offset LDR/STR (`ldr_imm`/`str_imm`);
            //   * offset in the signed imm9 range (−256..=255) → unscaled
            //     non-writeback LDUR/STUR (`ldur`/`stur`);
            //   * otherwise → materialize the effective address into a scratch
            //     (IP0/X16) and use a zero-offset access.
            Arm64Instruction::Ldr { rt, rn, offset } => {
                if *offset >= 0 && *offset % 8 == 0 && *offset <= 32760 {
                    // Cast: range-checked on the line above, and `ldr_imm`
                    // re-checks and refuses rather than masking.
                    if !emitter.ldr_imm(r_zr(*rt), r(*rn), *offset as u16) {
                        return None;
                    }
                } else if (-256..=255).contains(offset) {
                    emitter.ldur(r_zr(*rt), r(*rn), *offset as i16);
                } else {
                    // Out of imm9 range: IP0 = base + offset, then LDR [IP0, #0].
                    if !emit_addr_into_ip0(&mut emitter, r(*rn), *offset) {
                        return None;
                    }
                    emitter.ldur(r_zr(*rt), crate::aarch64::Reg::X16, 0);
                }
            }
            Arm64Instruction::Ldrb { rt, rn, offset } => {
                // Byte loads use an UNSCALED imm12 (units of 1), and only the
                // non-negative unsigned-offset form is encodable here. The one
                // caller is the safepoint poll, which uses offset 0.
                if *offset < 0 || *offset > 0xFFF {
                    return None;
                }
                // Cast: bounds-checked immediately above.
                if !emitter.ldrb_imm(r_zr(*rt), r(*rn), *offset as u16) {
                    return None;
                }
            }
            Arm64Instruction::Str { rt, rn, offset } => {
                if *offset >= 0 && *offset % 8 == 0 && *offset <= 32760 {
                    // Cast: range-checked on the line above; `str_imm` refuses
                    // anything it cannot encode exactly.
                    if !emitter.str_imm(r_zr(*rt), r(*rn), *offset as u16) {
                        return None;
                    }
                } else if (-256..=255).contains(offset) {
                    emitter.stur(r_zr(*rt), r(*rn), *offset as i16);
                } else {
                    // Out of imm9 range: IP0 = base + offset, then STR [IP0, #0].
                    //
                    // A4. `emit_addr_into_ip0` leaves the effective address in
                    // X16 and the store then reads `rt` -- so if `rt` IS X16,
                    // the value being stored was overwritten by its own
                    // address and this writes the address to memory. The
                    // `AndImm` arm further down already guards the sibling case
                    // (`rn.0 == 16`); this is the same hazard on the other
                    // operand, and the claim that "IP0 is never a regalloc
                    // output" is true of the ALLOCATOR but not of this
                    // backend's own X16 uses -- the stack-bang probe, the
                    // switch scratch and the poll/frame-record address
                    // materialisation all put things there.
                    if rt.0 == 16 {
                        return None;
                    }
                    if !emit_addr_into_ip0(&mut emitter, r(*rn), *offset) {
                        return None;
                    }
                    emitter.stur(r_zr(*rt), crate::aarch64::Reg::X16, 0);
                }
            }
            Arm64Instruction::Ldp {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                // The pseudo-op carries an `i32` and the encoder takes an
                // `i16`, so the conversion is a refusal point of its own: `as
                // i16` would wrap +32768 to -32768 before `ldp` ever sees it.
                let Ok(off) = i16::try_from(*offset) else {
                    return None;
                };
                if !emitter.ldp(r_zr(*rt1), r_zr(*rt2), r(*rn), off) {
                    return None;
                }
            }
            Arm64Instruction::Stp {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                let Ok(off) = i16::try_from(*offset) else {
                    return None;
                };
                if !emitter.stp(r_zr(*rt1), r_zr(*rt2), r(*rn), off) {
                    return None;
                }
            }
            // Bug-fix (ARM64 BUG #2): writeback prologue/epilogue pair ops.
            Arm64Instruction::StpPre {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                let Ok(off) = i16::try_from(*offset) else {
                    return None;
                };
                if !emitter.stp_pre(r_zr(*rt1), r_zr(*rt2), r(*rn), off) {
                    return None;
                }
            }
            Arm64Instruction::LdpPost {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                let Ok(off) = i16::try_from(*offset) else {
                    return None;
                };
                if !emitter.ldp_post(r_zr(*rt1), r_zr(*rt2), r(*rn), off) {
                    return None;
                }
            }
            Arm64Instruction::LdrLiteral { rt, label } => {
                // Emit a real LDR (literal) instruction. The offset to the
                // target label will be patched after all code is emitted.
                let pos = emitter.ldr_literal_x(r_zr(*rt));
                literal_patches.push((pos, *label));
            }

            // -- Branch --
            Arm64Instruction::B { label } => {
                let pos = emitter.b(0);
                branch_patches.push((pos, *label, false));
            }
            Arm64Instruction::BCond { cond, label } => {
                let pos = emitter.b_cond(to_cond(cond), 0);
                branch_patches.push((pos, *label, true));
            }
            Arm64Instruction::Bl { label } => {
                let pos = emitter.bl(0);
                branch_patches.push((pos, *label, false));
            }
            Arm64Instruction::Br { rn } => {
                emitter.br(r_zr(*rn));
            }
            Arm64Instruction::Blr { rn } => {
                emitter.blr(r_zr(*rn));
            }
            Arm64Instruction::Ret => {
                emitter.ret_lr();
            }
            Arm64Instruction::Cbz { rt, label } => {
                let pos = emitter.cbz(r_zr(*rt), 0);
                branch_patches.push((pos, *label, true));
            }
            Arm64Instruction::Cbnz { rt, label } => {
                let pos = emitter.cbnz(r_zr(*rt), 0);
                branch_patches.push((pos, *label, true));
            }

            // -- FP move --
            Arm64Instruction::FmovToFp { vd, rn } => {
                emitter.fmov_d_from_gp(fp(*vd), r_zr(*rn));
            }
            Arm64Instruction::FmovFromFp { rd, vn } => {
                emitter.fmov_gp_from_d(r_zr(*rd), fp(*vn));
            }
            Arm64Instruction::FmovFp { vd, vn } => {
                emitter.fmov_d(fp(*vd), fp(*vn));
            }

            // -- FP negate --
            Arm64Instruction::FnegDouble { vd, vn } => {
                emitter.fneg_d(fp(*vd), fp(*vn));
            }

            // -- FP single-precision --
            Arm64Instruction::FaddSingle { vd, vn, vm } => {
                emitter.fadd_s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FsubSingle { vd, vn, vm } => {
                emitter.fsub_s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FmulSingle { vd, vn, vm } => {
                emitter.fmul_s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FdivSingle { vd, vn, vm } => {
                emitter.fdiv_s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FcmpSingle { vn, vm } => {
                emitter.fcmp_s(fp(*vn), fp(*vm));
            }
            Arm64Instruction::FnegSingle { vd, vn } => {
                emitter.fneg_s(fp(*vd), fp(*vn));
            }

            // -- Conversion --
            Arm64Instruction::ScvtfDouble { vd, rn } => {
                emitter.scvtf_d_x(fp(*vd), r_zr(*rn));
            }
            Arm64Instruction::ScvtfSingle { vd, rn } => {
                emitter.scvtf_s_w(fp(*vd), r_zr(*rn));
            }
            Arm64Instruction::FcvtzsInt { rd, vn } => {
                emitter.fcvtzs_x_d(r_zr(*rd), fp(*vn));
            }
            Arm64Instruction::FcvtzsSingle { rd, vn } => {
                emitter.fcvtzs_w_s(r_zr(*rd), fp(*vn));
            }
            Arm64Instruction::FcvtSingleToDouble { vd, vn } => {
                emitter.fcvt_d_s(fp(*vd), fp(*vn));
            }
            Arm64Instruction::FcvtDoubleToSingle { vd, vn } => {
                emitter.fcvt_s_d(fp(*vd), fp(*vn));
            }

            // -- FP Load / Store --
            //
            // Bug-fix (aarch64 parity audit 2026-08-01, FP twin of ARM64 BUG #1):
            // the previous lowering was `*offset as u16` straight into the
            // SCALED UNSIGNED-offset form. Every FP spill slot is at a NEGATIVE
            // offset from FP (`Arm64FrameLayout::spill_offset` is always < 0),
            // and `-24i32 as u16` is 65512, which the encoder then scales by 8
            // — a load/store roughly 64 KiB ABOVE FP, i.e. into the caller's
            // frame. Silent memory corruption on every `fstore`/`fload` of a
            // spilled float or double.
            //
            // Routing now mirrors the GPR `Ldr`/`Str` arms exactly:
            //   * offset >= 0, correctly scaled, in range → scaled unsigned form
            //   * offset in the signed imm9 range (−256..=255) → unscaled,
            //     non-writeback LDUR/STUR (FP variants)
            //   * otherwise → materialize the address into IP0 (X16) and use a
            //     zero-offset access.
            Arm64Instruction::FpLdr {
                vt,
                rn,
                offset,
                is_double,
            } => {
                // Scaled unsigned form: imm12 scaled by the access size, so the
                // reachable byte range is 8*4095 for D and 4*4095 for S.
                let (scale, max_scaled) = if *is_double {
                    (8i32, 32760i32)
                } else {
                    (4, 16380)
                };
                if *offset >= 0 && *offset % scale == 0 && *offset <= max_scaled {
                    // Cast: range-checked on the line above, and the encoder
                    // re-checks and refuses rather than masking.
                    let ok = if *is_double {
                        emitter.ldr_fp_d(fp(*vt), r(*rn), *offset as u16)
                    } else {
                        emitter.ldr_fp_s(fp(*vt), r(*rn), *offset as u16)
                    };
                    if !ok {
                        return None;
                    }
                } else if (-256..=255).contains(offset) {
                    if *is_double {
                        emitter.ldur_fp_d(fp(*vt), r(*rn), *offset as i16);
                    } else {
                        emitter.ldur_fp_s(fp(*vt), r(*rn), *offset as i16);
                    }
                } else {
                    if !emit_addr_into_ip0(&mut emitter, r(*rn), *offset) {
                        return None;
                    }
                    if *is_double {
                        emitter.ldur_fp_d(fp(*vt), crate::aarch64::Reg::X16, 0);
                    } else {
                        emitter.ldur_fp_s(fp(*vt), crate::aarch64::Reg::X16, 0);
                    }
                }
            }
            Arm64Instruction::FpStr {
                vt,
                rn,
                offset,
                is_double,
            } => {
                let (scale, max_scaled) = if *is_double {
                    (8i32, 32760i32)
                } else {
                    (4, 16380)
                };
                if *offset >= 0 && *offset % scale == 0 && *offset <= max_scaled {
                    // Cast: range-checked on the line above; the encoder
                    // refuses anything it cannot encode exactly.
                    let ok = if *is_double {
                        emitter.str_fp_d(fp(*vt), r(*rn), *offset as u16)
                    } else {
                        emitter.str_fp_s(fp(*vt), r(*rn), *offset as u16)
                    };
                    if !ok {
                        return None;
                    }
                } else if (-256..=255).contains(offset) {
                    if *is_double {
                        emitter.stur_fp_d(fp(*vt), r(*rn), *offset as i16);
                    } else {
                        emitter.stur_fp_s(fp(*vt), r(*rn), *offset as i16);
                    }
                } else {
                    if !emit_addr_into_ip0(&mut emitter, r(*rn), *offset) {
                        return None;
                    }
                    if *is_double {
                        emitter.stur_fp_d(fp(*vt), crate::aarch64::Reg::X16, 0);
                    } else {
                        emitter.stur_fp_s(fp(*vt), crate::aarch64::Reg::X16, 0);
                    }
                }
            }

            // -- NEON FP (double) --
            Arm64Instruction::FaddDouble { vd, vn, vm } => {
                emitter.fadd_d(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FsubDouble { vd, vn, vm } => {
                emitter.fsub_d(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FmulDouble { vd, vn, vm } => {
                emitter.fmul_d(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FdivDouble { vd, vn, vm } => {
                emitter.fdiv_d(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FcmpDouble { vn, vm } => {
                emitter.fcmp_d(fp(*vn), fp(*vm));
            }

            // -- NEON SIMD (integer vector, 4x32) --
            Arm64Instruction::NeonLd1_4s { vt, rn } => {
                emitter.ld1_4s(fp(*vt), r(*rn));
            }
            Arm64Instruction::NeonSt1_4s { vt, rn } => {
                emitter.st1_4s(fp(*vt), r(*rn));
            }
            Arm64Instruction::NeonAdd4s { vd, vn, vm } => {
                emitter.add_v4s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::NeonMul4s { vd, vn, vm } => {
                emitter.mul_v4s(fp(*vd), fp(*vn), fp(*vm));
            }

            // -- 32-bit (W) integer forms --
            Arm64Instruction::AddW { rd, rn, rm } => emitter.add_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::SubW { rd, rn, rm } => emitter.sub_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::MulW { rd, rn, rm } => emitter.mul_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::AndW { rd, rn, rm } => emitter.and_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::OrrW { rd, rn, rm } => emitter.orr_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::EorW { rd, rn, rm } => emitter.eor_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::LslW { rd, rn, rm } => emitter.lsl_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::LsrW { rd, rn, rm } => emitter.lsr_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::AsrW { rd, rn, rm } => emitter.asr_w(r_gp(*rd), r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::NegW { rd, rn } => emitter.neg_w(r_gp(*rd), r_gp(*rn)),
            Arm64Instruction::SDivW { rd, rn, rm } => {
                emitter.sdiv_w(r_gp(*rd), r_gp(*rn), r_gp(*rm))
            }
            Arm64Instruction::MsubW { rd, rn, rm, ra } => {
                emitter.msub_w(r_gp(*rd), r_gp(*rn), r_gp(*rm), r_gp(*ra))
            }
            Arm64Instruction::AddImmW { rd, rn, imm } => {
                if !emit_addsub_imm_w(&mut emitter, r_gp(*rd), r_gp(*rn), *imm, false) {
                    return None;
                }
            }
            Arm64Instruction::SubImmW { rd, rn, imm } => {
                if !emit_addsub_imm_w(&mut emitter, r_gp(*rd), r_gp(*rn), *imm, true) {
                    return None;
                }
            }
            Arm64Instruction::CmpW { rn, rm } => emitter.cmp_w(r_gp(*rn), r_gp(*rm)),
            Arm64Instruction::CmpImmW { rn, imm } => {
                emit_w_cmp_const(&mut emitter, r_gp(*rn), *imm)
            }
            Arm64Instruction::CbzW { rt, label } => {
                let pos = emitter.cbz_w(r_zr(*rt), 0);
                branch_patches.push((pos, *label, true));
            }
            Arm64Instruction::CbnzW { rt, label } => {
                let pos = emitter.cbnz_w(r_zr(*rt), 0);
                branch_patches.push((pos, *label, true));
            }
            Arm64Instruction::Sxtw { rd, rn } => emitter.sxtw(r_gp(*rd), r_gp(*rn)),
            Arm64Instruction::Sxth { rd, rn } => emitter.sxth(r_gp(*rd), r_gp(*rn)),
            Arm64Instruction::Sxtb { rd, rn } => emitter.sxtb(r_gp(*rd), r_gp(*rn)),
            Arm64Instruction::AndImm { rd, rn, imm } => {
                if !emitter.and_imm(r_gp(*rd), r_gp(*rn), *imm) {
                    if rn.0 == 16 {
                        return None;
                    }
                    emitter.mov_imm64(crate::aarch64::Reg::X16, *imm);
                    emitter.and(r_gp(*rd), r_gp(*rn), crate::aarch64::Reg::X16);
                }
            }
            Arm64Instruction::Cset { rd, cond } => emitter.cset(r_gp(*rd), to_cond(cond)),
            Arm64Instruction::Cneg { rd, rn, cond } => {
                emitter.cneg(r_gp(*rd), r_gp(*rn), to_cond(cond));
            }

            // -- FP width forms --
            Arm64Instruction::FmovToFpSingle { vd, rn } => {
                emitter.fmov_s_from_w(fp(*vd), r_zr(*rn))
            }
            Arm64Instruction::FmovFromFpSingle { rd, vn } => {
                emitter.fmov_w_from_s(r_zr(*rd), fp(*vn))
            }
            Arm64Instruction::FmovFpSingle { vd, vn } => emitter.fmov_s(fp(*vd), fp(*vn)),
            Arm64Instruction::ScvtfDoubleW { vd, rn } => emitter.scvtf_d_w(fp(*vd), r_zr(*rn)),
            Arm64Instruction::ScvtfSingleX { vd, rn } => emitter.scvtf_s_x(fp(*vd), r_zr(*rn)),
            Arm64Instruction::FcvtzsIntW { rd, vn } => emitter.fcvtzs_w_d(r_zr(*rd), fp(*vn)),
            Arm64Instruction::FcvtzsSingleX { rd, vn } => emitter.fcvtzs_x_s(r_zr(*rd), fp(*vn)),

            // -- Switches --
            Arm64Instruction::TableSwitch {
                key,
                low,
                default,
                targets,
            } => {
                use crate::aarch64::{Cond, Reg};
                let Some(max_index) = targets
                    .len()
                    .checked_sub(1)
                    .and_then(|m| u32::try_from(m).ok())
                else {
                    return None;
                };
                // X17 = key - low, wrapping at 32 bits, so a key below `low`
                // becomes a huge index and fails the unsigned check below.
                emit_w_sub_const(&mut emitter, Reg::X17, r_gp(*key), *low);
                if max_index <= 0xFFF {
                    // Cast: range-checked.
                    emitter.cmp_imm_w(Reg::X17, max_index as u16);
                } else {
                    emitter.mov_imm64(Reg::X16, u64::from(max_index));
                    emitter.cmp_w(Reg::X17, Reg::X16);
                }
                let to_default = emitter.b_cond(Cond::HI, 0);
                branch_patches.push((to_default, *default, true));
                let adr = emitter.adr(Reg::X16, 0);
                emitter.ldrsw_reg_uxtw_scaled(Reg::X17, Reg::X16, Reg::X17);
                emitter.add(Reg::X16, Reg::X16, Reg::X17);
                emitter.br(Reg::X16);
                let table = emitter.offset();
                emitter.patch_adr(adr, table);
                for &label in targets {
                    let entry = emitter.emit_u32_data(0);
                    table_patches.push((entry, table, label));
                }
            }
            Arm64Instruction::LookupSwitch {
                key,
                pairs,
                default,
            } => {
                for &(value, label) in pairs {
                    emit_w_cmp_const(&mut emitter, r_gp(*key), value);
                    let pos = emitter.b_cond(crate::aarch64::Cond::EQ, 0);
                    branch_patches.push((pos, label, true));
                }
                let pos = emitter.b(0);
                branch_patches.push((pos, *default, false));
            }

            // -- Shared-memory access (round 9 wave 10) --
            //
            // Zero offset only: the pseudo-op's address is exactly `[rn]`. The
            // ordered forms have no offset field at all, and the plain forms
            // are given 0, which every unsigned-offset encoder accepts.
            Arm64Instruction::MemLoad {
                rt,
                rn,
                width,
                acquire,
            } => {
                if *acquire {
                    match width {
                        Arm64MemWidth::B8 => emitter.ldarb(r_zr(*rt), r(*rn)),
                        Arm64MemWidth::H16 => emitter.ldarh(r_zr(*rt), r(*rn)),
                        Arm64MemWidth::W32 => emitter.ldar_w(r_zr(*rt), r(*rn)),
                        Arm64MemWidth::X64 => emitter.ldar(r_zr(*rt), r(*rn)),
                    }
                } else {
                    let ok = match width {
                        Arm64MemWidth::B8 => emitter.ldrb_imm(r_zr(*rt), r(*rn), 0),
                        Arm64MemWidth::H16 => emitter.ldrh_imm(r_zr(*rt), r(*rn), 0),
                        Arm64MemWidth::W32 => emitter.ldr_imm_w(r_gp(*rt), r_gp(*rn), 0),
                        Arm64MemWidth::X64 => emitter.ldr_imm(r_zr(*rt), r(*rn), 0),
                    };
                    if !ok {
                        return None;
                    }
                }
            }
            Arm64Instruction::MemStore {
                rt,
                rn,
                width,
                release,
            } => {
                if *release {
                    match width {
                        Arm64MemWidth::B8 => emitter.stlrb(r_zr(*rt), r(*rn)),
                        Arm64MemWidth::H16 => emitter.stlrh(r_zr(*rt), r(*rn)),
                        Arm64MemWidth::W32 => emitter.stlr_w(r_zr(*rt), r(*rn)),
                        Arm64MemWidth::X64 => emitter.stlr(r_zr(*rt), r(*rn)),
                    }
                } else {
                    let ok = match width {
                        Arm64MemWidth::B8 => emitter.strb_imm(r_zr(*rt), r(*rn), 0),
                        Arm64MemWidth::H16 => emitter.strh_imm(r_zr(*rt), r(*rn), 0),
                        Arm64MemWidth::W32 => emitter.str_imm_w(r_gp(*rt), r_gp(*rn), 0),
                        Arm64MemWidth::X64 => emitter.str_imm(r_zr(*rt), r(*rn), 0),
                    };
                    if !ok {
                        return None;
                    }
                }
            }
            Arm64Instruction::DmbIsh => {
                // CRm = 0b1011 (ISH).
                emitter.dmb(0b1011);
            }

            // -- System --
            Arm64Instruction::Nop => {
                emitter.nop();
            }
            Arm64Instruction::Brk { imm } => {
                emitter.brk(*imm);
            }
        }
    }

    // Patch branches.
    //
    // An unbound label is NOT recoverable here. The emitted placeholder is a
    // branch with displacement 0 — i.e. a branch to itself, an infinite loop —
    // and nothing downstream patches it: `jit::try_compile_inner`'s aarch64
    // arm copies these bytes straight into an `ExecutableBuffer` and hands the
    // entry point to the VM. The previous code deliberately left the
    // placeholder "for unresolved calls"; since `emit_invoke` always fails the
    // method and `compile_method_with_info`'s discovery pass now binds every
    // back-edge target, an unbound label can only mean a branch target that is
    // not a valid instruction boundary (malformed or truncated bytecode), so
    // the only safe action is to discard the method and interpret it.
    for &(offset, label, is_cond) in &branch_patches {
        let target = match label_offsets.get(&label) {
            Some(&t) => t,
            None => return None,
        };
        if is_cond {
            emitter.patch_bcond(offset, target);
        } else {
            emitter.patch_branch(offset, target);
        }
    }

    // Patch LDR literal instructions to point to their target labels.
    // The label should reference a position containing a 64-bit constant
    // (e.g. a literal pool entry appended after all code). Same reasoning as
    // above: an unpatched LDR-literal reads whatever happens to sit at
    // `pc + 0`, which is the instruction itself — garbage, not a constant.
    for &(offset, label) in &literal_patches {
        let target = match label_offsets.get(&label) {
            Some(&t) => t,
            None => return None,
        };
        emitter.patch_ldr_literal(offset, target);    }

    // Patch jump-table words: each is the signed distance from the table base
    // to its case, which the dispatch adds to the base it loaded with `ADR`.
    for &(entry, base, label) in &table_patches {
        let target = match label_offsets.get(&label) {
            Some(&t) => t,
            None => return None,
        };
        let Ok(delta) = i32::try_from(target as i64 - base as i64) else {
            return None;
        };
        // Cast: the two's-complement word `LDRSW` sign-extends back.
        emitter.patch_u32(entry, delta as u32);
    }

    // Sticky encoding-overflow check — see this function's doc comment. Must
    // come AFTER the patch loops: `patch_branch`/`patch_bcond`/
    // `patch_ldr_literal` are themselves able to trip the flag, and in a
    // release build they are the *only* thing that reports an out-of-range
    // patch (the `debug_assert!` inside `mark_branch_overflow` is compiled
    // out).
    if emitter.overflowed() {
        return None;
    }

    // A5. The same discipline for a register that could not be encoded: `r`
    // and `fp` noted it and kept going, and the whole body is discarded here.
    // Before this, both helpers `.expect()`ed and a regalloc bug killed the
    // process instead of costing one method its compiled form.
    if take_invalid_register() {
        return None;
    }

    // The sentinel: a safepoint recorded at the very end of the stream has a
    // following pseudo-op index of `instructions.len()`, whose byte offset is
    // the end of the code.
    pseudo_offsets.push(emitter.offset());
    Some((emitter.code().to_vec(), pseudo_offsets))
}

/// Encode a compiled method.
pub fn emit_machine_code(result: &Arm64CompileResult) -> Option<Vec<u8>> {
    emit_machine_code_inner(result).map(|(code, _)| code)
}

/// This frame's storage-class partition, for the GC's band verifier.
///
/// Without it `CompiledMethod::frame_layout` stays all-zero, and a zero layout
/// tells the verifier that NOTHING is a register image -- so the prologue's
/// saved FP/LR pair and the caller's saved X19-X28 all count as in-band words
/// of THIS frame. Those hold the CALLER's live references, which this frame's
/// maps have no business naming, so the oracle would report them `never_mapped`
/// and refute a coverage claim on pure noise. An oracle that cries wolf is
/// worse than one that is off.
///
/// The saved FP/LR pair is the frame record AT FP (`[FP]`, `[FP+8]`), outside
/// the `[FP - size, FP)` band altogether -- where x86-64 keeps its saved RBP and
/// return address. Below FP come the callee-saved GPRs, and the spill area
/// BELOW those, which is the mirror of x86-64's order. `callee_saved_shallow`
/// says so, so the verifier uses the range exclusion instead of x86-64's
/// "everything at or beyond `callee_saved_lo`" half-line, which here would
/// swallow the spill area -- the one region the oop maps describe.
///
/// Offsets are positive, meaning `[FP - off]`, matching the x64 convention the
/// consumer expects.
pub(in crate::aarch64_backend) fn arm64_frame_layout(frame: &Arm64FrameLayout) -> crate::FrameLayout {
    let mut out = crate::FrameLayout::default();
    // Register images: the callee-saved GPRs, `[FP-8]` down to
    // `[FP-callee_save_bytes]`. With none saved there is no image in the band.
    if !frame.saved_regs.is_empty() {
        out.callee_saved_lo = 8;
        // `-callee_save_offset` is the DEEPEST saved-register offset; `+8`
        // makes the range half-open over it.
        out.callee_saved_hi = (-frame.callee_save_offset) + 8;
    }
    out.callee_saved_shallow = true;
    // The spill area: frame-homed locals, then operand slots, then the
    // safepoint homes and the sp-id word. Every one of those is described by
    // the dataflow or the operand marks, which is what lets the verifier treat
    // a word the active map does not name as DEAD rather than missed.
    if frame.num_spills > 0 {
        let deepest = -frame.spill_offset; // slot 0 is the deepest word
        let shallowest = -(frame.spill_offset + (frame.num_spills as i32 - 1) * 8);
        out.spill_lo = shallowest;
        out.spill_hi = deepest + 8;
    }
    // `java_locals_hi` is deliberately left 0: this backend has no
    // `[FP - (i+1)*8]` local convention -- a local is either in a callee-saved
    // register or in the spill area above.
    out
}

/// Build the publishable artifact for an aarch64 compilation: encode it, and
/// attach the oop maps the GC will read.
///
/// # Why this lives here and not at the call site
///
/// Its caller in `try_compile_inner` sits behind
/// `#[cfg(target_arch = "aarch64")]`, so on an x86-64 developer host or CI
/// runner that block is not compiled AT ALL -- it is never type-checked, never
/// linted and never tested. That is how the publication path came to drop
/// `oop_maps` on the floor without anything noticing: it built its
/// `CompiledMethod` with `CompiledMethod::new(buf)` and never transferred the
/// backend's maps, so even a correct map writer would have produced nothing
/// observable.
///
/// Keeping the logic in a function with no `cfg` on it means every `cargo test`
/// run on any host compiles and exercises it (see
/// `tests::a_published_artifact_carries_its_resolved_oop_maps`), and the gated
/// caller shrinks to one line that cannot silently rot.
pub fn publish_compiled_method(result: &Arm64CompileResult) -> Option<crate::CompiledMethod> {
    let (machine_code, oop_maps) = emit_machine_code_with_oop_maps(result)?;
    // `new_in`, not `new`: the process-wide JIT code arena carves this body
    // out of a shared executable reservation instead of taking its own
    // `VirtualAlloc`/`mmap`. Behaviour is byte-identical while
    // `CRATONVM_JIT_CODE_ARENA` is unset, because `new_in` falls straight
    // through to `new` then — so this call site is what makes the flag
    // reachable, not what changes the default.
    let mut buf = crate::ExecutableBuffer::new_in(
        crate::platform::shared_jit_code_arena(),
        machine_code.len().max(4096),
    )?;
    buf.set_tag("aarch64-backend");
    buf.emit(&machine_code);
    // `try_new`: a refused RW->RX flip declines the compile instead of
    // aborting the process (round 9 wave 3).
    //
    // ...and `try_new_with_context` when the body takes the VM context pointer
    // (round 9 wave 16). The two constructors differ in `needs_context` and in
    // nothing else, and that flag is what makes the VM enter through
    // `try_call_with_context` -- which passes the context in X0, which is
    // exactly where this body's prologue homes it from. Getting this wrong in
    // either direction shifts every Java argument by one register.
    let mut cm = if result.needs_context {
        crate::CompiledMethod::try_new_with_context(buf)?
    } else {
        crate::CompiledMethod::try_new(buf)?
    };
    // The transfer that was missing. `has_precise_oop_maps()` becomes true when
    // this is non-empty, which makes the walker enumerate these slots IN
    // ADDITION to its conservative sweep -- strictly additive, because
    // suppressing the sweep is gated on `fully_oop_covered`, which this backend
    // never sets -- not for want of the safepoint-id slot, which exists now,
    // but because suppressing the conservative scan is a claim no non-aarch64
    // host can earn.
    cm.oop_maps = oop_maps;
    // The slot the maps above are keyed through. Without it the runtime's
    // `active_safepoint_id` returns `None` and no map can be selected by id.
    cm.sp_id_slot_off = result.sp_id_slot_off;
    // The storage-class partition the band verifier needs to tell this frame's
    // words from the caller's saved registers. Publishing a zero layout would
    // make the oracle report the caller's live references as this frame's
    // missed roots.
    cm.frame_layout = arm64_frame_layout(&result.frame);
    // THE SIZE THAT LAYOUT IS READ OVER (round 9 wave 21), and the third
    // member of this wave's family: a map that is published and unreadable.
    //
    // `osr_frame_size` is named for OSR and is not about OSR -- "normal
    // compiled entries populate it too", says the band walker, and x64's
    // single-pass driver sets it from its own `frame_size`. It is how a
    // compiled frame says which words are ITS OWN: everything that walks one
    // (`scan_compiled_frame_bands`, `remap_active_jit_frames`,
    // `verify_precise_covers_conservative`) reads `[rbp - osr_frame_size, rbp)`
    // and REFUSES THE FRAME when the field is `0`. This backend never set it,
    // so no aarch64 frame was ever band-walked, remapped or verified -- which
    // is also why arming the oracle would have found nothing to look at.
    //
    // The value is the distance from FP down to SP, which is what the prologue
    // subtracts: `STP X29, X30, [SP, #-16]!` puts FP 16 bytes below the old
    // SP, and `SUB SP, SP, #(frame_size - 16)` is the rest. `frame_size`
    // itself would reach 16 bytes PAST SP into the callee's frame, and a false
    // positive there is exactly what `arm64_frame_layout` exists to prevent.
    // The saved FP/LR pair sits AT `[FP]`/`[FP+8]`, above the band, which is
    // where x86-64 keeps its saved RBP and return address.
    cm.osr_frame_size = result.frame.frame_size.saturating_sub(16);
    // The declaring classes of every static this body reads directly (round 9
    // wave 10), as the x64 tier records them: the interpreter's compiled-entry
    // path ensure-initializes them once per artifact before first execution.
    cm.static_init_classes = result.static_init_classes.clone();
    // FULLY OOP COVERED -- the claim that lets the collector SUPPRESS its
    // conservative scan of these frames, so every term is a thing that had to
    // be built rather than assumed:
    //
    //   * an id slot, or `active_safepoint_id` returns `None` and no map can be
    //     selected at all;
    //   * at least one map, so the claim is not vacuously true for a method
    //     that emitted no safepoint (a method with no poll is not "covered",
    //     it is unobserved);
    //   * one map per safepoint, so every id resolves -- `find_oop_map_for_safepoint_id`
    //     finding nothing is indistinguishable from a frame that is not covered;
    //   * and NO safepoint that failed to describe what was live at it
    //     (`incomplete_oop_maps`), which is a count and not a set of bytecode
    //     pcs, because two safepoints can share one bci and a set lets a
    //     complete map mask an incomplete one beside it.
    //
    //   * and A PUBLISHED FRAME BASE (`frame_base_published`, round 9 wave
    //     21). Every term above is addressed from it -- the id at
    //     `[frame_base - sp_id_slot_off]`, each slot at `[frame_base - off]` --
    //     and the runtime learns it only from `emit_frame_record`. Without
    //     that call `PreciseFrameInfo::exact_rbp` stays `0` and every precise
    //     path declines the frame, so the maps are unreadable and a coverage
    //     claim over them describes nothing. It is reachable: the helper slot
    //     is `0` under `CRATONVM_NO_PRECISE_JIT_MAPS=1` with no moving young
    //     generation.
    //
    // WHAT IT STILL RESTS ON, stated because it is the whole risk: the
    // operand-oop marks are exact BY CONSTRUCTION (lockstep push/pop, `dup`
    // carrying its mark, and references entering only through `aconst_null` and
    // `aload*`), and the local oop-ness comes from the flow-sensitive dataflow.
    // `CRATONVM_DBG_VERIFY_OOP_MAPS` is the runtime oracle that turns it from a
    // construction into evidence, and it arms itself on this backend rather
    // than waiting to be remembered (see `verify_oop_maps_enabled`).
    //
    // ROUND 9 WAVE 21 DROPPED `polls_enabled`. Wave 18 added it to withhold the
    // claim from a method whose only safepoints are allocation calls, on the
    // grounds that it was the strongest claim the backend makes and that wave
    // was not the wave to widen it in. The argument for widening was already
    // written there and is unchanged -- with polls off, the frame can be
    // stopped only inside a call, and every call this backend emits either
    // records a map (`emit_helper_call_at_safepoint`: the allocation helpers,
    // `jit_aaload`, `jit_aastore_type_check`, `jit_aastore`) or carries a
    // specific argument for why it cannot safepoint (`emit_helper_call`: the
    // field helpers and `dispatch_threw`; `jit_frame_record`, which writes one
    // thread-local; the NPE, AIOOBE, arithmetic and `athrow` stubs, which
    // allocate nothing and run no Java, because the exception object is built
    // later by the interpreter's drain on a stack these frames have left).
    //
    // What changed is that the claim was not yet WORTH widening: wave 18 also
    // shipped the frame-record gap above, so an allocation-only method's maps
    // could not be selected in the first place and a coverage bit over them
    // would have been exactly the vacuous claim
    // `bug-oop-map-coverage-bit-is-presence-not-completeness-20260820-FIXED.md`
    // warns about. With the base published the claim is about something.
    cm.fully_oop_covered = result.frame_base_published
        && result.sp_id_slot_off != 0
        && !cm.oop_maps.is_empty()
        && cm.oop_maps.len() == result.safepoint_count
        && result.incomplete_oop_maps == 0;
    Some(cm)
}

/// Encode a compiled method AND resolve its oop maps to real byte offsets.
///
/// This is the only way an `OopMapEntry` is ever produced on this backend. The
/// compiler records [`Arm64PendingOopMap`]s keyed by pseudo-op index because a
/// byte offset is not knowable until the encoder has run: the pseudo-op stream
/// is not fixed-width (`Label`/`Comment` emit nothing, `ConstantPoolEntry`
/// emits 8 bytes, `MovImm`/`AddImm`/`CmpImm` and out-of-range `Ldr`/`Str`
/// expand to 1-4 words). Keying a map by `instruction_count * 4` -- what this
/// backend used to do before the writer was made fail-closed in the 2026-08-01
/// parity audit -- lands the GC on the WRONG FRAME SLOTS at a real safepoint.
///
/// Fails closed: a pending map whose `pseudo_index` is not a valid index
/// discards the whole method rather than publish a map with a guessed PC.
pub fn emit_machine_code_with_oop_maps(
    result: &Arm64CompileResult,
) -> Option<(Vec<u8>, Vec<crate::OopMapEntry>)> {
    let (code, pseudo_offsets) = emit_machine_code_inner(result)?;
    let mut maps: Vec<crate::OopMapEntry> = Vec::with_capacity(result.pending_oop_maps.len());
    for pending in &result.pending_oop_maps {
        let idx = pending.pseudo_index as usize;
        // `pseudo_offsets` carries the trailing sentinel, so a safepoint at the
        // very end of the stream is in range; anything past that is a bug in
        // the writer, not a method we may publish a map for.
        let Some(&byte_off) = pseudo_offsets.get(idx) else {
            return None;
        };
        let Ok(native_pc_offset) = u32::try_from(byte_off) else {
            return None;
        };
        maps.push(crate::OopMapEntry {
            native_pc_offset,
            // The safepoint id this map belongs to, and the reason the frame
            // stamps the same value into its sp-id slot: this is what
            // `find_oop_map_for_safepoint_id` matches on, so the collector
            // selects the map for the site the frame is ACTUALLY standing at
            // rather than a union over every safepoint in the method.
            bytecode_pc: pending.safepoint_id,
            frame_slot_offsets: pending.frame_slot_offsets.clone(),
            // No shadow stack and no relocation support on this backend, and
            // register-resident oops are covered only by the CONSERVATIVE walk
            // (see `emit_oop_map_for_safepoint`) -- which marks but cannot
            // rewrite. Claiming moving-young coverage here would be the exact
            // false claim `relocation_coverage_complete` exists to prevent.
            moving_young_coverage_complete: false,
            // AArch64 publishes no blind GPR spill image, so there is no
            // slot set for a mask to narrow. `None` is the honest answer.
            reg_oop_mask: None,
            live_frame_hi: 0,
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            non_oop_stack_slots: Vec::new(),
            stack_marks_exact: false,
            shadow_pushed: 0,
            // No blind image here either (gcg g3/v).
            spill_written: None,
        });
    }
    Some((code, maps))
}

