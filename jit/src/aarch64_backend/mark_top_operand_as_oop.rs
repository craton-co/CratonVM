#[allow(unused_imports)]
use super::*;

impl Arm64Backend {
    pub fn new() -> Self {
        Self {
            buffer: Arm64CodeBuffer::new(),
            frame: None,
            local_regs: Vec::new(),
            float_local_regs: Vec::new(),
            operand_stack: Vec::new(),
            held: Vec::new(),
            label_states: HashMap::new(),
            reachable: true,
            stack_kinds: crate::x64::stack_kinds::StackKindMap::default(),
            pc_labels: HashMap::new(),
            cur_bytecode_pc: 0,
            local_oop_masks: Vec::new(),
            local_oop_reached: Vec::new(),
            param_oop_mask: 0,
            param_jvm_slots: None,
            return_narrow: None,
            static_fields: HashMap::new(),
            instance_fields: HashMap::new(),
            needs_context: false,
            allocates: false,
            new_sites: HashMap::new(),
            multianewarray_sites: HashMap::new(),
            static_init_classes: Vec::new(),
            exception_table_empty: false,
            arith_throw_labels: Vec::new(),
            npe_throw_labels: Vec::new(),
            aioobe_stubs: Vec::new(),
            pending_local_oop_slots: Vec::new(),
            pending_operand_oop_slots: Vec::new(),
            epilogue_label: 0,
            num_params: 0,
            method_info: HashMap::new(),
            failed: false,
            pending_oop_maps: Vec::new(),
            // SAFETY: `JitRuntimeHelpers` is a plain struct of `usize`
            // addresses; all-zero is its documented "nothing wired" state, and
            // `safepoint_flag_addr == 0` is what gates the poll emitter.
            helpers: unsafe { std::mem::zeroed() },
            back_edge_targets: std::collections::HashSet::new(),
            sp_id_slot_off: 0,
            spill_area: Arm64SpillArea::default(),
            safepoint_count: 0,
            incomplete_oop_maps: 0,
            pending_map_incomplete: false,
            invoke_sites: HashMap::new(),
            typecheck_sites: HashMap::new(),
            calls: false,
            frame_base_published: false,
            safepoints_enabled: arm64_safepoints_enabled(),
        }
    }

    /// T1.1.3 — mark the top of the operand stack as holding an object
    /// reference. Production pushes set the mark from the entry's kind; this
    /// remains for tests that build a stack by hand.
    #[allow(dead_code)]
    pub(in crate::aarch64_backend) fn mark_top_operand_as_oop(&mut self) {
        if let Some(top) = self.operand_stack.last_mut() {
            top.oop = true;
        }
    }

    /// Record this safepoint's oop map: the frame slots that hold object
    /// references right now, keyed so the encoder can give them a real PC.
    ///
    /// The PC an `OopMapEntry` needs is a BYTE OFFSET, and this backend has a
    /// pseudo-op stream whose entries are not 4 bytes each, so the map is
    /// recorded against the pseudo-op INDEX of the instruction that follows
    /// the safepoint, in an [`Arm64PendingOopMap`], and
    /// [`emit_machine_code_with_oop_maps`] translates it.
    ///
    /// Names: reference operands already in their depth slots, the reference
    /// operands the poll stored for its call (`pending_operand_oop_slots`),
    /// and the reference locals it staged (`pending_local_oop_slots`). A
    /// reference operand still in a REGISTER is not nameable; the poll stores
    /// every one before calling this, which is why it is the only caller.
    pub(in crate::aarch64_backend) fn emit_oop_map_for_safepoint(&mut self, safepoint_id: u32) {
        if self.failed {
            return;
        }
        // The pseudo-op that will FOLLOW this safepoint: the "return address"
        // convention x64 records.
        let pseudo_index = match u32::try_from(self.buffer.instruction_count()) {
            Ok(n) => n,
            Err(_) => {
                self.failed = true;
                return;
            }
        };

        let mut offsets: Vec<i32> = self
            .operand_stack
            .iter()
            .filter(|o| o.oop)
            .filter_map(|o| match o.loc {
                OperandLoc::Slot(off) => Some(off),
                OperandLoc::Reg(_) => None,
            })
            .collect();
        offsets.extend(std::mem::take(&mut self.pending_operand_oop_slots));
        offsets.extend(std::mem::take(&mut self.pending_local_oop_slots));

        let mut slots: Vec<i16> = Vec::new();
        for off in offsets {
            match i16::try_from(off) {
                Ok(off16) => {
                    if !slots.contains(&off16) {
                        slots.push(off16);
                    }
                }
                // A slot further than `i16` from FP. There is no completeness
                // channel for this, so refuse rather than publish a map that
                // silently drops a live reference.
                Err(_) => {
                    self.failed = true;
                    return;
                }
            }
        }
        // PUBLISH EVERY SAFEPOINT, even one with no live reference: an id whose
        // map is absent is indistinguishable from an uncovered frame.
        self.safepoint_count += 1;
        if std::mem::take(&mut self.pending_map_incomplete) {
            self.incomplete_oop_maps += 1;
        }
        self.pending_oop_maps.push(Arm64PendingOopMap {
            pseudo_index,
            frame_slot_offsets: slots,
            safepoint_id,
        });
    }

    // -- The operand model ----------------------------------------------------

    /// Whether some operand-stack entry lives in `reg`.
    pub(in crate::aarch64_backend) fn reg_is_live(&self, reg: Arm64Register) -> bool {
        self.operand_stack
            .iter()
            .any(|o| o.loc == OperandLoc::Reg(reg))
    }

    /// A scratch register of the requested class that holds nothing the
    /// current bytecode still needs. The register is HELD until the next
    /// bytecode.
    ///
    /// When every register of the class is live or held, the DEEPEST
    /// register-located entry is moved to its depth slot first. The entry
    /// records the move, so its value is found again by depth -- never by
    /// asking which register it used to be in.
    pub(in crate::aarch64_backend) fn alloc_reg(&mut self, fp: bool) -> Arm64Register {
        let pool: &'static [Arm64Register] = if fp {
            &FLOAT_SCRATCH_REGS
        } else {
            &SCRATCH_REGS
        };
        if let Some(&reg) = pool
            .iter()
            .find(|&&r| !self.held.contains(&r) && !self.reg_is_live(r))
        {
            self.held.push(reg);
            return reg;
        }
        let victim = self.operand_stack.iter().position(|o| match o.loc {
            OperandLoc::Reg(r) => pool.contains(&r) && !self.held.contains(&r),
            OperandLoc::Slot(_) => false,
        });
        let Some(depth) = victim else {
            // Every register of the class is held by this one bytecode. No
            // bytecode needs that many; refuse rather than alias.
            self.failed = true;
            return pool[0];
        };
        let OperandLoc::Reg(reg) = self.operand_stack[depth].loc else {
            self.failed = true;
            return pool[0];
        };
        if !self.spill_entry(depth) {
            return pool[0];
        }
        self.held.push(reg);
        reg
    }

    /// Move entry `depth` from its register into its depth slot. `false` (and
    /// `failed`) when the slot is outside the reserved operand area.
    pub(in crate::aarch64_backend) fn spill_entry(&mut self, depth: usize) -> bool {
        let Some(entry) = self.operand_stack.get(depth).copied() else {
            self.failed = true;
            return false;
        };
        let OperandLoc::Reg(reg) = entry.loc else {
            return true;
        };
        let Some(offset) = self.spill_offset_for_depth(depth) else {
            self.failed = true;
            return false;
        };
        self.emit_store_kind(reg, entry.kind, offset);
        self.operand_stack[depth].loc = OperandLoc::Slot(offset);
        true
    }

    /// Put every entry in its depth slot: the one layout that every path into
    /// a branch target agrees on.
    pub(in crate::aarch64_backend) fn spill_all(&mut self) {
        for depth in 0..self.operand_stack.len() {
            if !self.spill_entry(depth) {
                return;
            }
        }
    }

    /// Store `reg` (holding a `kind`) to `[FP + offset]` at the kind's width.
    pub(in crate::aarch64_backend) fn emit_store_kind(&mut self, reg: Arm64Register, kind: OperandKind, offset: i32) {
        let inst = match kind {
            OperandKind::F32 | OperandKind::F64 => Arm64Instruction::FpStr {
                vt: reg,
                rn: Arm64Register::FP,
                offset,
                is_double: kind == OperandKind::F64,
            },
            _ => Arm64Instruction::Str {
                rt: reg,
                rn: Arm64Register::FP,
                offset,
            },
        };
        self.buffer.emit(inst);
    }

    /// Load a `kind` from `[FP + offset]` into `reg` at the kind's width.
    pub(in crate::aarch64_backend) fn emit_load_kind(&mut self, reg: Arm64Register, kind: OperandKind, offset: i32) {
        let inst = match kind {
            OperandKind::F32 | OperandKind::F64 => Arm64Instruction::FpLdr {
                vt: reg,
                rn: Arm64Register::FP,
                offset,
                is_double: kind == OperandKind::F64,
            },
            _ => Arm64Instruction::Ldr {
                rt: reg,
                rn: Arm64Register::FP,
                offset,
            },
        };
        self.buffer.emit(inst);
    }

    /// Push a value the current bytecode computed into `reg`.
    pub(in crate::aarch64_backend) fn push_reg(&mut self, kind: OperandKind, reg: Arm64Register) {
        self.operand_stack.push(Operand::in_reg(kind, reg));
    }

    /// Push `entry`'s kind and oop mark, now living in `reg`.
    pub(in crate::aarch64_backend) fn push_like(&mut self, entry: Operand, reg: Arm64Register) {
        self.operand_stack.push(Operand {
            loc: OperandLoc::Reg(reg),
            ..entry
        });
    }

    /// Pop the top entry into a register, reloading it from its slot when it
    /// was spilled. The register is HELD for the rest of this bytecode.
    /// Underflow sets `failed` and answers `None`.
    pub(in crate::aarch64_backend) fn pop_entry(&mut self) -> Option<(Arm64Register, Operand)> {
        let Some(entry) = self.operand_stack.pop() else {
            self.failed = true;
            return None;
        };
        match entry.loc {
            OperandLoc::Reg(reg) => {
                self.held.push(reg);
                Some((reg, entry))
            }
            OperandLoc::Slot(offset) => {
                let reg = self.alloc_reg(entry.kind.is_fp());
                self.emit_load_kind(reg, entry.kind, offset);
                Some((reg, entry))
            }
        }
    }

    /// Pop an entry that must be a `want`. Any other kind is a malformed
    /// method (the verifier would reject it) or a model bug; either refuses.
    pub(in crate::aarch64_backend) fn pop_kind(&mut self, want: OperandKind) -> Arm64Register {
        let fallback = if want.is_fp() {
            Arm64Register::V0
        } else {
            Arm64Register::X0
        };
        match self.pop_entry() {
            Some((reg, entry)) if entry.kind == want => reg,
            Some(_) => {
                self.failed = true;
                fallback
            }
            None => fallback,
        }
    }

    /// Pop an integer-class (int, long or reference) entry. Returns X0 as a
    /// sentinel and sets `self.failed = true` on underflow or an FP entry.
    pub fn pop_operand(&mut self) -> Arm64Register {
        match self.pop_entry() {
            Some((reg, entry)) if !entry.kind.is_fp() => reg,
            Some(_) => {
                self.failed = true;
                Arm64Register::X0
            }
            None => Arm64Register::X0,
        }
    }

    /// Discard the top entry without materializing it.
    pub(in crate::aarch64_backend) fn drop_top(&mut self) {
        if self.operand_stack.pop().is_none() {
            self.failed = true;
        }
    }

    /// Re-establish the sign-extended `int` form after a W-form producer.
    pub(in crate::aarch64_backend) fn emit_sxtw(&mut self, reg: Arm64Register) {
        self.buffer
            .emit(Arm64Instruction::Sxtw { rd: reg, rn: reg });
    }

    /// The kind and oop mark of every entry, bottom first.
    pub(in crate::aarch64_backend) fn stack_shape(&self) -> Vec<(OperandKind, bool)> {
        self.operand_stack.iter().map(|o| (o.kind, o.oop)).collect()
    }

    /// Record `shape` for target `pc`, or refuse the method when a different
    /// path already recorded a different one.
    pub(in crate::aarch64_backend) fn check_or_record_shape(&mut self, pc: usize, shape: Vec<(OperandKind, bool)>) {
        match self.label_states.get(&pc) {
            Some(existing) if *existing != shape => self.failed = true,
            Some(_) => {}
            None => {
                self.label_states.insert(pc, shape);
            }
        }
    }

    /// Everything a branch to `target` must do before it is emitted: put the
    /// stack in its canonical all-slots layout, record that shape, and resolve
    /// the label. Only STORES are emitted, so flags and held registers survive.
    pub(in crate::aarch64_backend) fn prepare_branch(&mut self, target: usize) -> u32 {
        self.spill_all();
        let shape = self.stack_shape();
        self.check_or_record_shape(target, shape);
        self.label_for_pc(target)
    }

    /// The walk is about to fall into branch target `pc`: arrive in the layout
    /// the branches to it use.
    pub(in crate::aarch64_backend) fn arrive_at_target(&mut self, pc: usize) {
        self.spill_all();
        let shape = self.stack_shape();
        self.check_or_record_shape(pc, shape);
    }

    /// Rebuild the model at a pc control cannot fall into: from the shape a
    /// branch recorded, else from the shared stack-kind analysis. With neither
    /// the pc is unreachable, and it is lowered against an empty stack -- at
    /// worst an underflow refuses the method.
    pub(in crate::aarch64_backend) fn restore_stack_at(&mut self, pc: usize) {
        let shape = match self.label_states.get(&pc) {
            Some(s) => Some(s.clone()),
            None => self.shape_from_analysis(pc),
        };
        self.operand_stack.clear();
        let Some(shape) = shape else {
            return;
        };
        for (depth, (kind, oop)) in shape.into_iter().enumerate() {
            let Some(offset) = self.spill_offset_for_depth(depth) else {
                self.failed = true;
                return;
            };
            self.operand_stack.push(Operand {
                kind,
                loc: OperandLoc::Slot(offset),
                oop,
            });
        }
    }

    /// The operand-stack shape at `pc` according to the shared analysis, or
    /// `None` when it has no answer or an entry is untyped.
    pub(in crate::aarch64_backend) fn shape_from_analysis(&self, pc: usize) -> Option<Vec<(OperandKind, bool)>> {
        use crate::x64::stack_kinds::StackKind;
        self.stack_kinds
            .get(pc)?
            .iter()
            .map(|k| match k {
                StackKind::Int => Some((OperandKind::I32, false)),
                StackKind::Long => Some((OperandKind::I64, false)),
                StackKind::Float => Some((OperandKind::F32, false)),
                StackKind::Double => Some((OperandKind::F64, false)),
                StackKind::Ref => Some((OperandKind::Ref, true)),
                StackKind::Unknown => None,
            })
            .collect()
    }

    /// Supply the runtime helper addresses this backend needs for a safepoint
    /// poll. Without it `safepoint_flag_addr` stays 0 and
    /// [`Self::emit_safepoint_poll`] emits nothing.
    pub fn set_helpers(&mut self, helpers: crate::JitRuntimeHelpers) {
        self.helpers = helpers;
    }

    /// Supply the resolved `invoke*` sites (round 9 wave 22). Without this
    /// every `invoke*` refuses, which is what this backend did from its first
    /// commit until that wave.
    pub fn set_invoke_site_info(&mut self, sites: HashMap<usize, Arm64InvokeSite>) {
        self.invoke_sites = sites;
    }

    /// Supply the resolved `checkcast`/`instanceof` sites (round 9 wave 23).
    pub fn set_typecheck_site_info(&mut self, sites: HashMap<usize, Arm64TypecheckSite>) {
        self.typecheck_sites = sites;
    }

    /// Override the safepoint-poll decision for this compilation.
    ///
    /// Exists because [`arm64_safepoints_enabled`] latches a `OnceLock`, so a
    /// test binary can only ever observe one arm of it.
    pub fn set_safepoints_enabled(&mut self, on: bool) {
        self.safepoints_enabled = on;
    }

    /// Seed the reference-parameter mask and the argument-to-slot layout from
    /// this method's descriptor.
    ///
    /// Must be called BEFORE compiling. The mask feeds
    /// `compute_local_oop_masks` and the entry poll. The slot layout is what
    /// the prologue homes each argument by: the VM passes ONE register per
    /// argument, and a `long`/`double` argument occupies TWO JVM local slots,
    /// so argument `i` is local `i` only while every earlier argument is
    /// category 1. Without a descriptor the identity layout is assumed, which
    /// is right for exactly those signatures.
    pub fn set_method_descriptor(&mut self, descriptor: &str, is_static: bool) {
        self.param_oop_mask = crate::compute_param_oop_mask(descriptor, is_static);
        self.param_jvm_slots = Some(crate::compute_param_jvm_slots(descriptor, is_static).0);
        self.return_narrow = crate::narrowed_int_return_tag(descriptor);
    }

    /// Supply the resolved `getstatic` sites, keyed by the pc of each `0xb2`
    /// (round 9 wave 10). Must be called BEFORE compiling. A site that is
    /// absent, or whose field is not a primitive, refuses the method; see
    /// [`Arm64StaticField`] for what each entry promises and
    /// [`static_field_sites`] for the walk that finds the pcs.
    pub fn set_static_field_info(&mut self, fields: HashMap<usize, Arm64StaticField>) {
        self.static_fields = fields;
    }

    /// Resolved `getfield`/`putfield` sites, keyed by the pc of the opcode
    /// (round 9 wave 15). A site absent from the map refuses the method, which
    /// is what an unwired caller gets for free: the map is empty until this is
    /// called.
    pub fn set_instance_field_info(&mut self, fields: HashMap<usize, Arm64InstanceField>) {
        self.instance_fields = fields;
    }

    /// Resolved `new`/`anewarray` sites, keyed by the pc of the opcode (round
    /// 9 wave 18). A site absent from the map refuses the method, which is
    /// what an unwired caller gets for free: the map is empty until this is
    /// called. `newarray` is not here -- its `atype` is an operand byte, so
    /// there is nothing for a caller to resolve.
    pub fn set_new_site_info(&mut self, sites: HashMap<usize, Arm64NewSite>) {
        self.new_sites = sites;
    }

    /// Supply the resolved `multianewarray` sites (h23c), keyed by the pc of
    /// the `0xc5`, each the packed `(holder_class_id | cp_idx << 32)`
    /// descriptor `cratonvm_jit::pack_multianewarray_site` builds.
    ///
    /// The descriptor carries no ARITY -- the helper takes that separately --
    /// so one map serves every dimension count, and the backend reads the
    /// count from the opcode's own operand byte.
    pub fn set_multianewarray_site_info(&mut self, sites: HashMap<usize, i64>) {
        self.multianewarray_sites = sites;
    }

    /// Tell the backend whether this method's exception table is empty
    /// (round 9 wave 11). Must be called BEFORE compiling; the caller passes
    /// `cached.exception_table.is_empty()`.
    ///
    /// The one fact the direct-throw lowerings need that the bytecode does not
    /// carry. `idiv`/`irem`/`ldiv`/`lrem` compile only when it is `true`
    /// (and `helpers.throw_arithmetic` is wired): the zero-divisor stub leaves
    /// this frame through its epilogue and the interpreter's JIT-return drain
    /// raises the exception with an UNKNOWN throw pc, which is exact only when
    /// no handler in this method could have caught it.
    pub fn set_exception_table_empty(&mut self, empty: bool) {
        self.exception_table_empty = empty;
    }

    /// `jit_set_throw_bci(bci)` — the x64 twin is the stamp every trap stub
    /// there emits (`x64/deopt_stubs.rs`) before returning the `i64::MIN`
    /// deopt sentinel through the epilogue. The interpreter's JIT-return
    /// drain routes a pending exception through THIS method's own exception
    /// table by bci; without the stamp it has only an unknown return pc,
    /// which is exact only when the table is empty (nothing to route to) --
    /// the condition every trap lowering here used to require outright.
    ///
    /// A no-op when [`Self::exception_table_empty`] holds: propagation is
    /// unconditionally correct with nothing to catch, and the stamp is a
    /// helper call this backend has no reason to pay on that path. Also a
    /// no-op when the helper is unwired, matching this backend's usual
    /// "unwired means byte-identical to before" contract -- callers that gate
    /// on [`Self::can_route_exception`] never reach a live trap edge in that
    /// case anyway.
    pub(in crate::aarch64_backend) fn emit_stamp_throw_bci(&mut self, bci: usize) {
        if self.exception_table_empty || self.helpers.set_throw_bci == 0 {
            return;
        }
        let Ok(bci) = i64::try_from(bci) else {
            self.failed = true;
            return;
        };
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X0,
            imm: bci,
        });
        // Cast: a helper address is a real mapped pointer, always < i64::MAX.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.set_throw_bci as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });
    }

    /// Whether a trapping lowering can leave through the epilogue with the
    /// `i64::MIN` deopt sentinel and have the interpreter's drain route it
    /// correctly: either nothing in this method could catch it
    /// ([`Self::exception_table_empty`]), or [`Self::emit_stamp_throw_bci`]
    /// is available to tell the drain exactly which bci trapped, so it can
    /// search this method's own exception table the way the interpreter
    /// always has.
    ///
    /// Replaces the plain `exception_table_empty` refusal every trapping
    /// lowering here carried before the stamp existed. A caller that gates on
    /// this MUST call [`Self::emit_stamp_throw_bci`] on every edge that
    /// returns the sentinel -- this predicate only says the stamp is
    /// possible, not that a given call site remembered to emit it.
    pub(in crate::aarch64_backend) fn can_route_exception(&self) -> bool {
        self.exception_table_empty || self.helpers.set_throw_bci != 0
    }

    /// Can this compilation raise `ArithmeticException` from compiled code?
    ///
    /// Both halves are "not wired" refusals: without the helper there is no
    /// throw path, and without [`Self::can_route_exception`] the stub's throw
    /// pc could pick the wrong handler (or miss the right one). Either
    /// missing keeps the round-7 refusal, so an unwired caller is
    /// byte-identical to before.
    pub(in crate::aarch64_backend) fn can_throw_arithmetic(&self) -> bool {
        self.helpers.throw_arithmetic != 0 && self.can_route_exception()
    }

    /// The label of this pass's `ArithmeticException` stub for the CURRENT
    /// bci, creating it on first use.
    ///
    /// Keyed by bci, not shared across the whole method: once
    /// [`Self::emit_stamp_throw_bci`] is live (a non-empty exception table),
    /// two `idiv`s at different bcis that shared one stub would have the
    /// second one's throw routed through the first one's bci, silently
    /// picking the wrong handler or missing the right one. With an empty
    /// table every bci maps to the one stub anyway (the stamp is a no-op, so
    /// nothing distinguishes them) -- `self.exception_table_empty` collapses
    /// the key so that case keeps sharing one stub, unchanged from before
    /// this existed.
    pub(in crate::aarch64_backend) fn arith_throw_label(&mut self) -> u32 {
        let key = if self.exception_table_empty {
            usize::MAX
        } else {
            self.cur_bytecode_pc
        };
        if let Some(&(_, label)) = self.arith_throw_labels.iter().find(|(b, _)| *b == key) {
            return label;
        }
        let label = self.buffer.new_label();
        self.arith_throw_labels.push((key, label));
        label
    }

    /// Emit every `ArithmeticException` stub this pass branched to, one per
    /// distinct bci (see [`Self::arith_throw_label`]). The aarch64 twin of
    /// x64's reason-3 deopt stub (`x64/deopt_stubs.rs`):
    ///
    /// ```text
    ///   throw_<bci>:
    ///     <emit_stamp_throw_bci(bci), if the exception table is not empty>
    ///     MOVZ/MOVK X16, #jit_throw_arithmetic
    ///     BLR       X16          ; sets the pending-arithmetic + deopt
    ///                            ; signals, returns i64::MIN in X0
    ///     B         epilogue     ; X0 survives the epilogue
    /// ```
    ///
    /// `jit_throw_arithmetic` takes no arguments, allocates nothing and runs
    /// no Java, so the call is not a safepoint: no operand is spilled and no
    /// oop map is recorded (x64 records none either). The operand stack and
    /// the caller-saved registers are dead -- this frame is leaving -- and the
    /// callee-saved local homes are restored by the epilogue, which is the
    /// same one every `*return` branches to. SP is 16-aligned here, as at the
    /// safepoint poll's `BLR`: the frame is sized in 16-byte units and
    /// nothing below it is pushed.
    pub(in crate::aarch64_backend) fn emit_arith_throw_stub(&mut self) {
        let stubs = std::mem::take(&mut self.arith_throw_labels);
        for (bci, label) in stubs {
            self.buffer.bind_label(label);
            if bci != usize::MAX {
                self.emit_stamp_throw_bci(bci);
            }
            // Cast: a helper address is a real mapped pointer, always < i64::MAX.
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X16,
                imm: self.helpers.throw_arithmetic as i64,
            });
            self.buffer.emit(Arm64Instruction::Blr {
                rn: Arm64Register::X16,
            });
            self.buffer.emit(Arm64Instruction::B {
                label: self.epilogue_label,
            });
        }
    }

    /// Can this compilation raise `NullPointerException` from compiled code?
    /// (Round 9 wave 12.)
    ///
    /// The same two-part condition as [`Self::can_throw_arithmetic`], for the
    /// same two reasons: without `helpers.jit_npe_with_action` there is no
    /// throw path at all, and without [`Self::can_route_exception`] the
    /// drain's `JitThrowPc::Unknown` could pick the wrong handler (or skip a
    /// `finally`). Either missing keeps the historical refusal, so an
    /// unwired caller is byte-identical to before.
    pub(in crate::aarch64_backend) fn can_throw_npe(&self) -> bool {
        self.helpers.jit_npe_with_action != 0 && self.can_route_exception()
    }

    /// The label of this pass's `NullPointerException` stub for JEP-358
    /// `action` at the CURRENT bci, creating it on first use.
    ///
    /// One stub per `(action, bci)`, not per action alone, once
    /// [`Self::emit_stamp_throw_bci`] is live: the helper's argument is only
    /// the action code, so two `arraylength`s on different receivers still
    /// want the same stub body, but with a non-empty exception table each
    /// needs its OWN throw-bci stamp ahead of it, or a null `arraylength` at
    /// bci 20 would route through whatever bci last shared this stub. With an
    /// empty table the stamp is a no-op and every bci collapses back onto one
    /// stub per action, unchanged from before this existed -- see
    /// [`Self::arith_throw_label`] for the identical reasoning.
    pub(in crate::aarch64_backend) fn npe_throw_label(&mut self, action: u8) -> u32 {
        let bci = if self.exception_table_empty {
            usize::MAX
        } else {
            self.cur_bytecode_pc
        };
        if let Some(&(_, _, label)) = self
            .npe_throw_labels
            .iter()
            .find(|(a, b, _)| *a == action && *b == bci)
        {
            return label;
        }
        let label = self.buffer.new_label();
        self.npe_throw_labels.push((action, bci, label));
        label
    }

    /// Emit every `NullPointerException` stub this pass branched to, after the
    /// epilogue's `RET`. The NPE twin of [`Self::emit_arith_throw_stub`]:
    ///
    /// ```text
    ///   npe_<action>_<bci>:
    ///     <emit_stamp_throw_bci(bci), if the exception table is not empty>
    ///     MOVZ/MOVK X0,  #action      ; the JEP-358 code, unpacked site id 0
    ///     MOVZ/MOVK X16, #jit_npe_with_action
    ///     BLR       X16               ; sets pending-NPE(action) + deopt
    ///     MOVZ      X0,  #i64::MIN    ; the deopt sentinel THIS stub owes
    ///     B         epilogue
    /// ```
    ///
    /// The one shape difference from the arithmetic stub is the last `MOV`:
    /// `jit_throw_arithmetic` RETURNS `i64::MIN` in X0, while
    /// `jit_npe_with_action` returns `()` (its x64 caller is a per-action stub
    /// that loads the sentinel itself, `x64::emit_null_check_store_stubs`), so
    /// the sentinel is this stub's to materialize. Emitting the call and
    /// forgetting the sentinel would return whatever the helper left in X0 as
    /// the method's result and drop the exception on the floor.
    ///
    /// The argument is the PACKED word `jit_npe_with_action` decodes: low byte
    /// the action, upper 24 bits an inline null-check site id. This backend has
    /// no `NpeTrapSite` table, so the id is 0 -- the helper's documented
    /// "stub that sets only the action" case, which reads back as
    /// `trap_key == 0` with no special casing.
    ///
    /// Not a safepoint, for the same reason the arithmetic stub is not: the
    /// helper allocates nothing and runs no Java, the operand stack is dead
    /// (this frame is leaving), and the epilogue restores the callee-saved
    /// homes.
    pub(in crate::aarch64_backend) fn emit_npe_throw_stubs(&mut self) {
        let stubs = std::mem::take(&mut self.npe_throw_labels);
        for (action, bci, label) in stubs {
            self.buffer.bind_label(label);
            if bci != usize::MAX {
                self.emit_stamp_throw_bci(bci);
            }
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X0,
                imm: i64::from(action),
            });
            // Cast: a helper address is a real mapped pointer, always < i64::MAX.
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X16,
                imm: self.helpers.jit_npe_with_action as i64,
            });
            self.buffer.emit(Arm64Instruction::Blr {
                rn: Arm64Register::X16,
            });
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X0,
                imm: i64::MIN,
            });
            self.buffer.emit(Arm64Instruction::B {
                label: self.epilogue_label,
            });
        }
    }

    /// `CBZ Xobj, npe_<action>` -- the inline null check every receiver
    /// dereference on this backend owes (round 9 wave 12).
    ///
    /// x64 reaches the same place through a `TEST`/`JZ` to a shared per-action
    /// stub. The 64-bit `CBZ` is right for a reference on aarch64 whether or
    /// not compressed oops are on: this backend never materializes a narrow
    /// oop -- every reference it holds is a full pointer, and null is the
    /// all-zero word.
    ///
    /// Callers must only use it where the exception is exact, i.e. under
    /// [`Self::can_throw_npe`].
    pub(in crate::aarch64_backend) fn emit_null_check(&mut self, obj: Arm64Register, action: u8) {
        let throw = self.npe_throw_label(action);
        self.buffer.emit(Arm64Instruction::Cbz {
            rt: obj,
            label: throw,
        });
    }

    /// `idiv`/`irem` (`kind == I32`) and `ldiv`/`lrem` (`I64`), round 9 wave
    /// 11. Only called when [`Self::can_throw_arithmetic`] holds.
    ///
    /// ```text
    ///     CBZ   Wb|Xb, throw      ; JVMS §6.5: a zero divisor throws
    ///     SDIV  Wd|Xd, Wa|Xa, Wb|Xb
    ///     MSUB  Wd|Xd, Wd|Xd, Wb|Xb, Wa|Xa   ; rem only: a - (a/b)*b
    ///     SXTW  Xd, Wd            ; int only: the canonical int form
    /// ```
    ///
    /// AArch64 `SDIV` never traps: the overflow case (`MIN / -1`) yields `MIN`
    /// and the remainder `MSUB` then yields 0, both exactly what JVMS
    /// `idiv`/`irem`/`ldiv`/`lrem` require, so -- unlike x64's `IDIV`, which
    /// faults -- no `-1` guard is owed. The zero test is the whole guard, and
    /// it is what the pre-round-7 lowering lacked (`BRK #1` for division,
    /// nothing at all for remainder, where `SDIV` by zero silently gave 0).
    pub(in crate::aarch64_backend) fn emit_int_div_rem(&mut self, kind: OperandKind, rem: bool) {
        let rhs = self.pop_kind(kind);
        let lhs = self.pop_kind(kind);
        let throw = self.arith_throw_label();
        let wide = kind == OperandKind::I64;
        self.buffer.emit(if wide {
            Arm64Instruction::Cbz {
                rt: rhs,
                label: throw,
            }
        } else {
            Arm64Instruction::CbzW {
                rt: rhs,
                label: throw,
            }
        });
        // `lhs`/`rhs` are HELD, so the result register is neither.
        let dst = self.alloc_reg(false);
        self.buffer.emit(if wide {
            Arm64Instruction::SDiv {
                rd: dst,
                rn: lhs,
                rm: rhs,
            }
        } else {
            Arm64Instruction::SDivW {
                rd: dst,
                rn: lhs,
                rm: rhs,
            }
        });
        if rem {
            self.buffer.emit(if wide {
                Arm64Instruction::Msub {
                    rd: dst,
                    rn: dst,
                    rm: rhs,
                    ra: lhs,
                }
            } else {
                Arm64Instruction::MsubW {
                    rd: dst,
                    rn: dst,
                    rm: rhs,
                    ra: lhs,
                }
            });
        }
        if !wide {
            self.emit_sxtw(dst);
        }
        self.push_reg(kind, dst);
    }

    /// Whether this compilation reserves a SAFEPOINT FRAME: the per-local
    /// home slots, the safepoint-id word, and the frame-base publication that
    /// addresses both.
    ///
    /// Two things want one: a method that POLLS, and -- since round 9 wave 18
    /// -- a method that ALLOCATES, because an allocation call stops this frame
    /// inside a callee that can move objects whether or not
    /// `CRATONVM_JIT_ARM64_SAFEPOINTS` is on.
    ///
    /// It is a predicate rather than three copies of the same disjunction
    /// because wave 18 wrote it twice and missed the third: the homes and the
    /// id word were widened to the allocating case, [`Self::emit_frame_record`]
    /// was not, and a frame base that is never published makes both useless --
    /// see that function.
    pub(in crate::aarch64_backend) fn wants_safepoint_frame(&self) -> bool {
        self.safepoints_enabled || self.allocates || self.calls
    }

    /// Publish this frame's base so the GC root walk can find it.
    ///
    /// The safepoint-id slot is read as `[frame_base - sp_id_slot_off]`, and
    /// the runtime learns `frame_base` from `set_top_frame_base`, which it is
    /// told through `helpers.frame_record`. FP is the base this backend
    /// publishes, playing the role x64's RBP does.
    ///
    /// **Round 9 wave 21: gated on [`Self::wants_safepoint_frame`], not on
    /// polls.** Wave 18 gave an allocating method the safepoint homes and the
    /// id word on the argument that an allocation stops the frame inside a
    /// moving callee either way -- and left this one behind. The consequence
    /// was the exact defect that argument was written to prevent, one level
    /// up: the runtime reads the id at `[frame_base - sp_id_slot_off]` and
    /// learns `frame_base` from HERE, so with no frame record
    /// `PreciseFrameInfo::exact_rbp` stays `0`, every precise path
    /// (`scan_active_oop_map_at_rbp`, `verify_precise_covers_conservative`,
    /// `moving_young_osr_method_needs_fallback`'s `exact_rbp != 0`) declines
    /// the frame, and the maps the allocation call had just recorded could
    /// never be read. The method was still CORRECT -- the conservative walk
    /// backs every one of those paths -- but a precise map that nothing can
    /// select is not precision, and the whole point of wave 18 was that an
    /// allocating frame can be stopped in a callee that MOVES objects, which
    /// the conservative walk marks without being able to rewrite.
    ///
    /// Emitted after the arguments are homed and before the entry poll. The
    /// call clobbers X0-X17 and V0-V7, which is sound only because nothing is
    /// on the operand stack yet; a non-empty stack here refuses the method
    /// rather than lose a value across the call. The VM context is already in
    /// its frame word by then (the prologue homes it out of X0), so clobbering
    /// X0 here costs nothing.
    pub(in crate::aarch64_backend) fn emit_frame_record(&mut self) {
        if self.failed || !self.wants_safepoint_frame() || self.helpers.frame_record == 0 {
            return;
        }
        if !self.operand_stack.is_empty() {
            self.failed = true;
            return;
        }
        self.buffer.emit(Arm64Instruction::Mov {
            rd: Arm64Register::X0,
            rm: Arm64Register::FP,
        });
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.frame_record as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });
        self.frame_base_published = true;
    }

    /// Emit a cooperative GC safepoint poll.
    ///
    /// ```text
    ///     MOVZ/MOVK X16, #safepoint_flag_addr
    ///     LDRB      W17, [X16]          ; ONE byte -- the flag is an AtomicBool
    ///     CBZ       X17, skip           ; clear -> no safepoint requested
    ///     <store every register-located operand, GPR and FP, to its slot>
    ///     <store register-homed reference locals to their homes>
    ///     <stamp the safepoint id>
    ///     MOVZ/MOVK X16, #safepoint_slow_path
    ///     BLR       X16
    ///     <oop map recorded at the return address>
    ///     <reload the locals and the operands>
    ///   skip:
    /// ```
    ///
    /// X16/X17 are IP0/IP1, which AAPCS64 reserves for exactly this and which
    /// hold no operand or local. Java locals live in X19-X28, which the call
    /// preserves. The operand stack lives in X9-X15 and V0-V7, which it does
    /// not -- hence the store and reload. Both classes: this used to spill
    /// only the GPR operands, so a live float or double operand in V0-V7 was
    /// destroyed by every taken poll.
    ///
    /// # Why the stores and reloads sit INSIDE the branch
    ///
    /// They leave the compile-time model exactly as it was: an operand that was
    /// in a register is in the same register again on both paths, so the model
    /// and the two runtime paths agree without a merge.
    ///
    /// # What the GC sees
    ///
    /// The map, recorded at the BLR's return address, names every reference
    /// operand (now in its depth slot) and every reference LOCAL: a frame-homed
    /// one where it lives, and a register-homed one at the home it was just
    /// stored to. Register homes are callee-saved, so the value would survive
    /// on its own -- but inside the CALLEE's save area, where only a
    /// conservative walk sees it, and a conservative walk cannot rewrite a
    /// relocated object. The reload afterwards is what carries a moved
    /// object's new address back into the register.
    pub(in crate::aarch64_backend) fn emit_safepoint_poll(&mut self, entry: bool) {
        if self.failed || !self.safepoints_enabled {
            return;
        }
        // The "not wired" contract, identical to x64's.
        if self.helpers.safepoint_flag_addr == 0 || self.helpers.safepoint_slow_path == 0 {
            return;
        }
        // THE SAFEPOINT ID: the site's bci, or the synthetic `ENTRY_POLL_BC_PC`
        // for the method-entry poll (bci 0 is a legal site of its own).
        let safepoint_id = if entry {
            crate::x64::safepoint::ENTRY_POLL_BC_PC as u32
        } else {
            match u32::try_from(self.cur_bytecode_pc) {
                Ok(n) => n,
                Err(_) => {
                    self.failed = true;
                    return;
                }
            }
        };
        let skip = self.buffer.new_label();
        // Cast: a helper address is a real mapped pointer, always < i64::MAX.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.safepoint_flag_addr as i64,
        });
        self.buffer.emit(Arm64Instruction::Ldrb {
            rt: Arm64Register::X17,
            rn: Arm64Register::X16,
            offset: 0,
        });
        self.buffer.emit(Arm64Instruction::Cbz {
            rt: Arm64Register::X17,
            label: skip,
        });

        // Store every register-located operand for the call. An operand that
        // is already in its slot needs nothing; the map writer names it.
        let Some(stored) = self.spill_operands_for_call() else {
            return;
        };
        // NAME THE REFERENCE LOCALS.
        let Some(reg_homed) = self.home_reference_locals_for_call(entry) else {
            return;
        };
        // Stamp the id BEFORE the call, so a collector that stops this thread
        // inside the slow path reads the site it is actually standing at.
        self.stamp_safepoint_id(safepoint_id);

        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.safepoint_slow_path as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });
        self.emit_oop_map_for_safepoint(safepoint_id);
        self.reload_after_safepoint(&reg_homed, stored);
        self.buffer.bind_label(skip);
    }

    /// Store every register-located operand to its depth slot, and record the
    /// frame offset of each reference one for the map.
    ///
    /// Shared by [`Self::emit_safepoint_poll`] and
    /// [`Self::emit_helper_call_at_safepoint`] (round 9 wave 18) — one copy of
    /// a sequence whose two halves (the store and the matching reload) have to
    /// agree, rather than two copies that can drift.
    ///
    /// The operand MODEL is left untouched: each entry still says `Reg`,
    /// because [`Self::reload_after_safepoint`] puts the value back in that
    /// same register. `None` (with `failed` set) when a live operand has no
    /// slot, which would leave it in a caller-saved register across a call.
    pub(in crate::aarch64_backend) fn spill_operands_for_call(&mut self) -> Option<Vec<(Arm64Register, OperandKind, i32)>> {
        let mut stored: Vec<(Arm64Register, OperandKind, i32)> = Vec::new();
        for depth in 0..self.operand_stack.len() {
            let operand = self.operand_stack[depth];
            let OperandLoc::Reg(reg) = operand.loc else {
                continue;
            };
            let Some(offset) = self.spill_offset_for_depth(depth) else {
                self.failed = true;
                return None;
            };
            self.emit_store_kind(reg, operand.kind, offset);
            if operand.oop {
                self.pending_operand_oop_slots.push(offset);
            }
            stored.push((reg, operand.kind, offset));
        }
        Some(stored)
    }

    /// Put every reference LOCAL where the collector can read and rewrite it,
    /// and record its frame offset for the map.
    ///
    /// A register-homed one is stored to its safepoint home and returned, so
    /// the caller can reload it after the call: the home is what a relocating
    /// collector rewrites, and without the reload the register keeps the old
    /// address. A frame-homed one is already in the right place and only has
    /// to be named.
    ///
    /// `None` (with `failed` set) when a live reference local has no home,
    /// which would leave it reachable only through a conservative scan — and a
    /// conservative scan marks but cannot rewrite.
    pub(in crate::aarch64_backend) fn home_reference_locals_for_call(&mut self, entry: bool) -> Option<Vec<(usize, i32)>> {
        let mut reg_homed: Vec<(usize, i32)> = Vec::new();
        let claim = self.oop_locals_at_current_pc(entry);
        if claim.is_none() && !self.local_regs.is_empty() {
            // The dataflow could not answer for this site (an unreached pc, or
            // more than 64 locals). Sound only while a conservative scan still
            // runs, so this method may not claim full coverage.
            self.pending_map_incomplete = true;
        }
        if let Some(mut mask) = claim {
            while mask != 0 {
                // Cast: count/index to usize
                let i = mask.trailing_zeros() as usize;
                mask &= mask - 1;
                if i >= self.local_regs.len() {
                    continue;
                }
                if let Some(reg) = self.local_regs.get(i).copied().flatten() {
                    let Some(off) = self.safepoint_home_for_reg_local(i) else {
                        self.failed = true;
                        return None;
                    };
                    self.buffer.emit(Arm64Instruction::Str {
                        rt: reg,
                        rn: Arm64Register::FP,
                        offset: off,
                    });
                    self.pending_local_oop_slots.push(off);
                    reg_homed.push((i, off));
                } else {
                    // Frame-homed: already where the GC can read and rewrite
                    // it. At the ENTRY poll that includes frame-homed reference
                    // PARAMETERS, which the argument homing stored before this
                    // poll runs.
                    let Some(off) = self.local_slot_offset(i) else {
                        self.failed = true;
                        return None;
                    };
                    self.pending_local_oop_slots.push(off);                }            }        }
        Some(reg_homed)    }

    /// Stamp `id` into the frame's safepoint-id word, so a collector that
    /// stops this thread inside the callee reads the site it is standing at.
    pub(in crate::aarch64_backend) fn stamp_safepoint_id(&mut self, id: u32) {
        if self.sp_id_slot_off == 0 {
            return;
        }
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X17,
            imm: i64::from(id),
        });
        self.buffer.emit(Arm64Instruction::Str {
            rt: Arm64Register::X17,
            rn: Arm64Register::FP,
            offset: -self.sp_id_slot_off,
        });
    }

    /// Reload what [`Self::home_reference_locals_for_call`] and
    /// [`Self::spill_operands_for_call`] put in the frame.
    ///
    /// The locals first, and the reason is the whole point of homing them: a
    /// relocating collector rewrote the HOME, and this is what carries the
    /// moved object's new address back into the register.
    pub(in crate::aarch64_backend) fn reload_after_safepoint(
        &mut self,
        reg_homed: &[(usize, i32)],
        stored: Vec<(Arm64Register, OperandKind, i32)>,
    ) {
        for (i, off) in reg_homed {
            if let Some(reg) = self.local_regs.get(*i).copied().flatten() {
                self.buffer.emit(Arm64Instruction::Ldr {
                    rt: reg,
                    rn: Arm64Register::FP,
                    offset: *off,
                });
            }
        }
        for (reg, kind, offset) in stored {
            self.emit_load_kind(reg, kind, offset);
        }
    }

    /// The oop-local mask in force at the safepoint being emitted, or `None`
    /// when no claim can be made.
    ///
    /// `None` is a REFUSAL, not "no oop locals": the dataflow is empty above 64
    /// locals and unreached at pcs only an exception edge can arrive at.
    pub(in crate::aarch64_backend) fn oop_locals_at_current_pc(&self, entry: bool) -> Option<u64> {
        if entry {
            // The entry poll runs before the walk; the live oops there are
            // exactly the reference parameters.
            return Some(self.param_oop_mask);
        }
        if self.local_oop_masks.is_empty() {
            return None;
        }
        if !self
            .local_oop_reached
            .get(self.cur_bytecode_pc)
            .copied()
            .unwrap_or(false)
        {
            return None;
        }
        self.local_oop_masks.get(self.cur_bytecode_pc).copied()
    }

    /// Frame offset of the safepoint home reserved for register-homed local
    /// `index`, or `None` if it has no register or no home was reserved.
    ///
    /// `k` numbers the register-homed locals in order; where that word sits is
    /// [`Arm64SpillArea::safepoint_home_word`]'s business, not this function's.
    /// A `k` past the reserved homes refuses rather than walking off the end of
    /// its region into the safepoint-id word.
    pub(in crate::aarch64_backend) fn safepoint_home_for_reg_local(&self, index: usize) -> Option<i32> {
        self.local_regs.get(index).copied().flatten()?;
        let k = (0..index)
            .filter(|&i| self.local_regs.get(i).copied().flatten().is_some())
            .count();
        self.spill_word_offset(self.spill_area.safepoint_home_word(k))
    }

    /// Frame offset of the word the prologue homed the VM context pointer
    /// into, or `None` when this compilation does not take a context.
    pub(in crate::aarch64_backend) fn context_slot_offset(&self) -> Option<i32> {
        self.spill_word_offset(self.spill_area.context_word())
    }

    /// Frame offset of the slot reserved for operand-stack depth `depth`, or
    /// `None` when `depth` is outside the operand area.
    ///
    /// `max_stack` counts JVM slots and every entry takes at least one, so
    /// every legal depth has a slot. A depth at or past `max_stack` would land
    /// on a safepoint home or the id word, so
    /// [`Arm64SpillArea::operand_word`] refuses it — bounded against the
    /// operand region, not against the grand total, which is the distinction
    /// that makes the refusal mean anything.
    pub(in crate::aarch64_backend) fn spill_offset_for_depth(&self, depth: usize) -> Option<i32> {
        self.spill_word_offset(self.spill_area.operand_word(depth))
    }

    /// Run the shared operand-stack kind analysis over `bytecode`.
    ///
    /// `static_fields` types each resolved `getstatic` (round 9 wave 10), so a
    /// pc after one is still described; an unresolved one leaves its
    /// successors `Unknown`, which is harmless because that site refuses the
    /// method anyway.
    pub(in crate::aarch64_backend) fn analyze_stack_kinds(
        bytecode: &[u8],
        static_fields: &HashMap<usize, Arm64StaticField>,
        instance_fields: &HashMap<usize, Arm64InstanceField>,
        invoke_sites: &HashMap<usize, Arm64InvokeSite>,
    ) -> crate::x64::stack_kinds::StackKindMap {
        use crate::x64::stack_kinds::{analyze, StackKindInputs};
        let refs = rustc_hash::FxHashSet::default();
        let inputs = StackKindInputs {
            // Round 9 wave 16: without these the analysis answers `None` from
            // the first `getfield` onwards (its `0xb4` arm needs the field's
            // type to say what it pushed), so every pc after one would be
            // undescribed -- and `restore_stack_at` would refuse to rebuild
            // the model at any branch target past it.
            field_types: instance_fields
                .iter()
                .map(|(&pc, f)| (pc, f.type_tag))
                .collect(),
            static_types: static_fields
                .iter()
                .map(|(&pc, f)| (pc, f.type_tag))
                .collect(),
            // Round 9 wave 22, and the same argument as the field types
            // above: the `0xb6..=0xba` arm answers `None` for a site it has no
            // shape for, which would leave every pc after the first call
            // undescribed. `calls` wants the PARAMETER count with no
            // receiver -- the opcode says whether there is one -- while
            // `num_jit_args` includes it, so the receiver comes back off here.
            calls: invoke_sites
                .iter()
                .map(|(&pc, site)| {
                    let receiver =
                        usize::from(!matches!(bytecode.get(pc), Some(0xb8) | Some(0xba)));
                    (
                        pc,
                        (site.num_jit_args.saturating_sub(receiver), site.return_type),
                    )
                })
                .collect(),
            ldc_refs: &refs,
            ldc_fp: &refs,
            ldc_resolved: &refs,
            handler_pcs: &[],
        };
        analyze(bytecode, bytecode.len(), &inputs)
    }

    /// Whether each of the top `want` entries is category 2, top first, or
    /// `None` when the stack holds fewer entries.
    ///
    /// The stack shuffles are specified in JVM SLOTS and categories, and this
    /// model has one entry per VALUE, typed. So each shuffle arm resolves its
    /// JVMS form from these categories, and a form the JVMS does not define
    /// (a `dup` of a `long`, say) refuses the method.
    pub(in crate::aarch64_backend) fn top_categories(&self, want: usize) -> Option<Vec<bool>> {
        let n = self.operand_stack.len();
        if n < want {
            return None;
        }
        Some(
            self.operand_stack[n - want..]
                .iter()
                .rev()
                .map(|o| o.kind.is_category2())
                .collect(),
        )
    }

    /// A fresh register holding a copy of the `kind` value in `reg`.
    pub(in crate::aarch64_backend) fn copy_value(&mut self, reg: Arm64Register, kind: OperandKind) -> Arm64Register {
        let dst = self.alloc_reg(kind.is_fp());
        let inst = match kind {
            OperandKind::F32 => Arm64Instruction::FmovFpSingle { vd: dst, vn: reg },
            OperandKind::F64 => Arm64Instruction::FmovFp { vd: dst, vn: reg },
            _ => Arm64Instruction::Mov { rd: dst, rm: reg },
        };
        self.buffer.emit(inst);
        dst
    }

    /// The shared duplicate shuffle, `[under.., group..] -> [group'.., under..,
    /// group..]`, counted in ENTRIES. Every `dup*` form is one of these once
    /// its categories are resolved: `dup` is (1, 0), `dup_x1` (1, 1), `dup2` of
    /// two category-1 values (2, 0), and so on.
    pub(in crate::aarch64_backend) fn emit_dup_group_over(&mut self, group_entries: usize, under_entries: usize) {
        let mut group = Vec::with_capacity(group_entries);
        for _ in 0..group_entries {
            match self.pop_entry() {
                Some(e) => group.push(e),
                None => return,
            }
        }
        let mut under = Vec::with_capacity(under_entries);
        for _ in 0..under_entries {
            match self.pop_entry() {
                Some(e) => under.push(e),
                None => return,
            }
        }
        let copies: Vec<Arm64Register> = group
            .iter()
            .map(|&(reg, e)| self.copy_value(reg, e.kind))
            .collect();
        for (&copy, &(_, e)) in copies.iter().zip(group.iter()).rev() {
            self.push_like(e, copy);
        }
        for &(reg, e) in under.iter().rev() {
            self.push_like(e, reg);
        }
        for &(reg, e) in group.iter().rev() {
            self.push_like(e, reg);
        }
    }

    /// Get or create a label for a bytecode PC.
    ///
    /// Also the single chokepoint where a **loop back-edge** is detected: every
    /// branch target on this backend -- `goto`, `if*`, and every switch case and
    /// default -- is resolved through here, so a target at or before the
    /// instruction being lowered is exactly the set of back-edges.
    ///
    /// A compiled loop needs a safepoint poll: without one a thread inside it
    /// never observes a stop-the-world request and any GC that needs to stop
    /// it hangs the VM. With polls on (`CRATONVM_JIT_ARM64_SAFEPOINTS`) the
    /// target is RECORDED and pass 2 emits a poll at the loop header. With
    /// polls off there is nothing to put there, so the method is refused and
    /// interpreted -- deliberately for provably terminating loops too, because
    /// the property that matters is bounded time to the next safepoint.
    pub(in crate::aarch64_backend) fn label_for_pc(&mut self, pc: usize) -> u32 {
        if pc <= self.cur_bytecode_pc {
            if self.safepoints_enabled {
                self.back_edge_targets.insert(pc);
            } else {
                self.failed = true;
            }
        }
        self.label_for_pc_unchecked(pc)
    }

    /// Label allocation without the back-edge check.
    ///
    /// Used only by the walk loop's pre-seed step, which binds a label at a PC
    /// the discovery pass already identified as a branch target. That step is
    /// not itself a branch (and at `pc == 0` the check would misfire).
    pub(in crate::aarch64_backend) fn label_for_pc_unchecked(&mut self, pc: usize) -> u32 {
        if let Some(&label) = self.pc_labels.get(&pc) {
            label
        } else {
            let label = self.buffer.new_label();
            self.pc_labels.insert(pc, label);
            label
        }
    }

    // -- Prologue / Epilogue ------------------------------------------------

    /// Emit the AAPCS64 prologue.
    ///
    /// ```text
    ///     STP  X29, X30, [SP, #-16]!   ; the frame record
    ///     ADD  X29, SP, #0             ; FP -> the record: [FP] = caller FP, [FP+8] = LR
    ///     <stack bang: SUB X16, SP, #off; STR XZR, [X16] per page crossed>
    ///     SUB  SP, SP, #(frame_size - 16)
    ///     STP/STR callee-saved GPRs at [FP - 8 ...]
    ///     <stamp the safepoint-id slot unset>
    /// ```
    ///
    /// # The frame record is the standard one now
    ///
    /// The previous prologue set FP to the caller's SP, leaving the saved pair
    /// at `[FP-16]`/`[FP-8]`. AAPCS64 (and Darwin, and every unwinder, and the
    /// frame-pointer walk in `vm/src/jit/helpers.rs`) expects FP to point AT the
    /// record: `[FP]` = caller's FP, `[FP+8]` = LR. A walk through one of these
    /// frames read the caller's LR as its FP. Every FP-relative offset is
    /// rebased accordingly in `Arm64FrameLayout::compute`.
    ///
    /// `ADD X29, SP, #0` and not `MOV X29, SP`: `Mov` lowers to `ORR`, where
    /// register 31 is XZR, so it would set FP to zero.
    ///
    /// # The stack bang
    ///
    /// Before SP moves, every page the frame will cross is touched, so stack
    /// exhaustion faults ON the guard page (recoverable) instead of a large
    /// `SUB SP` stepping clean past it into unrelated memory. x64 does the same
    /// (`emit_stack_bang_before_frame_alloc`). This replaces the old refusal
    /// of every frame of 4096 bytes or more.
    pub(in crate::aarch64_backend) fn emit_prologue(&mut self) {
        let Some(frame) = self.frame.as_ref() else {
            self.failed = true;
            return;
        };
        let frame_size = frame.frame_size;
        let callee_save_offset = frame.callee_save_offset;
        let saved_len = frame.saved_regs.len();

        self.buffer.emit(Arm64Instruction::StpPre {
            rt1: Arm64Register::FP,
            rt2: Arm64Register::LR,
            rn: Arm64Register::SP,
            offset: -16,
        });
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: Arm64Register::FP,
            rn: Arm64Register::SP,
            imm: 0,
        });

        let below = frame_size - 16;
        let Some(probes) = stack_bang_probe_offsets(below) else {
            self.failed = true;
            return;
        };
        for off in probes {
            self.buffer.emit(Arm64Instruction::SubImm {
                rd: Arm64Register::X16,
                rn: Arm64Register::SP,
                imm: off,
            });
            // Register 31 as a store's Rt is XZR: the probe writes zero.
            self.buffer.emit(Arm64Instruction::Str {
                rt: Arm64Register::XZR,
                rn: Arm64Register::X16,
                offset: 0,
            });
        }
        if below > 0 {
            self.buffer.emit(Arm64Instruction::SubImm {
                rd: Arm64Register::SP,
                rn: Arm64Register::SP,
                imm: below,
            });
        }

        // Save callee-saved registers used for locals (in pairs), below FP.
        let mut i = 0;
        while i + 1 < saved_len {
            // Cast: i < 10 (CALLEE_SAVED.len()).
            let offset = callee_save_offset + (i as i32) * 8;
            let frame = self.frame.as_ref().expect("frame present");
            let (rt1, rt2) = (frame.saved_regs[i], frame.saved_regs[i + 1]);
            self.buffer.emit(Arm64Instruction::Stp {
                rt1,
                rt2,
                rn: Arm64Register::FP,
                offset,
            });
            i += 2;
        }
        if i < saved_len {
            // Cast: i < 10.
            let offset = callee_save_offset + (i as i32) * 8;
            let rt = self.frame.as_ref().expect("frame present").saved_regs[i];
            self.buffer.emit(Arm64Instruction::Str {
                rt,
                rn: Arm64Register::FP,
                offset,
            });
        }

        // STAMP THE SAFEPOINT-ID SLOT with "this frame has not reached a
        // safepoint yet" (`SP_ID_UNSET_BC_PC`, which matches no map). Left
        // uninitialised it could read as a valid id. X17 is not an argument
        // register, so this is safe before the arguments are homed.
        if self.sp_id_slot_off != 0 {
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X17,
                imm: crate::x64::safepoint::SP_ID_UNSET_BC_PC as i64,
            });
            self.buffer.emit(Arm64Instruction::Str {
                rt: Arm64Register::X17,
                rn: Arm64Register::FP,
                offset: -self.sp_id_slot_off,
            });
        }
        // HOME THE VM CONTEXT POINTER (round 9 wave 16), which arrives in X0
        // ahead of every Java argument. It has to reach its frame word before
        // anything can clobber X0 -- the entry safepoint poll's `BLR` would,
        // and so would a `getfield`'s own helper call. Nothing above touches
        // X0: the stack bang and the sp-id stamp use X16/X17, and the
        // callee-saved saves read X19-X28.
        if self.needs_context {
            let Some(off) = self.context_slot_offset() else {
                // `needs_context` and the frame's context word are set from
                // the same field, so this is unreachable; refuse rather than
                // silently compile a body that reads an unwritten word.
                self.failed = true;
                return;
            };
            self.buffer.emit(Arm64Instruction::Str {
                rt: Arm64Register::X0,
                rn: Arm64Register::FP,
                offset: off,
            });
        }

        // The method-entry poll is NOT emitted here: X0-X7 still hold the
        // arguments until `emit_argument_homing`, and the poll's call may
        // destroy them. See `the_entry_poll_runs_after_the_argument_copy`.
    }

    /// Deposit each incoming argument in its JVM local's home.
    ///
    /// The VM passes one 64-bit register per ARGUMENT (`this` first), and an
    /// `int` arrives sign-extended, a `float` as its zero-extended bit pattern
    /// and a `double` as its bits. Argument `i` goes to JVM local
    /// `arg_slots[i]`, which differs from `i` after any `long`/`double`
    /// argument. A register home gets a `MOV`; a frame home gets a `STR` of
    /// the whole word, which the local's later loads read at their own width.
    ///
    /// The previous prologue moved X_i into local i's register and did nothing
    /// for a frame-homed local -- which every `float`/`double` local is, since
    /// the allocator never gives one a GPR -- so every FP parameter, and every
    /// parameter after a `long`/`double`, read garbage.
    pub(in crate::aarch64_backend) fn emit_argument_homing(&mut self, arg_slots: &[usize]) {
        // THE CONTEXT OCCUPIES X0 (round 9 wave 16), so every Java argument
        // moves up one register and only seven fit. A body that reads its
        // first argument from X0 while the VM passed a context there reads the
        // context pointer as that argument, which is why this shift and
        // `Arm64CompileResult::needs_context` are driven by the same field.
        let shift = usize::from(self.needs_context);
        if arg_slots.len() + shift > Arm64EntryConvention::INT_ARG_REGS.len() {
            // Arguments past the eighth arrive on the stack, which this
            // prologue does not read.
            self.failed = true;
            return;
        }
        for (i, &slot) in arg_slots.iter().enumerate() {
            let arg = Arm64EntryConvention::INT_ARG_REGS[i + shift];
            match self.local_regs.get(slot).copied() {
                Some(Some(home)) => {
                    if home != arg {
                        self.buffer
                            .emit(Arm64Instruction::Mov { rd: home, rm: arg });
                    }
                }
                Some(None) => {
                    let Some(offset) = self.local_slot_offset(slot) else {
                        self.failed = true;
                        return;
                    };
                    self.buffer.emit(Arm64Instruction::Str {
                        rt: arg,
                        rn: Arm64Register::FP,
                        offset,
                    });
                }
                // A parameter slot past `max_locals`: a malformed method.
                None => {
                    self.failed = true;
                    return;
                }
            }
        }
    }

    /// Emit the epilogue: restore the callee-saved GPRs, `ADD SP, X29, #0`
    /// (again not `MOV`, for the same register-31 reason), `LDP X29, X30,
    /// [SP], #16`, `RET`.
    pub(in crate::aarch64_backend) fn emit_epilogue(&mut self) {
        let Some(frame) = self.frame.as_ref() else {
            self.failed = true;
            return;
        };
        let callee_save_offset = frame.callee_save_offset;
        let saved = frame.saved_regs.clone();

        self.buffer.bind_label(self.epilogue_label);

        let mut i = 0;
        while i + 1 < saved.len() {
            // Cast: i < 10.
            let offset = callee_save_offset + (i as i32) * 8;
            self.buffer.emit(Arm64Instruction::Ldp {
                rt1: saved[i],
                rt2: saved[i + 1],
                rn: Arm64Register::FP,
                offset,
            });
            i += 2;
        }
        if i < saved.len() {
            // Cast: i < 10.
            let offset = callee_save_offset + (i as i32) * 8;
            self.buffer.emit(Arm64Instruction::Ldr {
                rt: saved[i],
                rn: Arm64Register::FP,
                offset,
            });
        }

        self.buffer.emit(Arm64Instruction::AddImm {
            rd: Arm64Register::SP,
            rn: Arm64Register::FP,
            imm: 0,
        });
        self.buffer.emit(Arm64Instruction::LdpPost {
            rt1: Arm64Register::FP,
            rt2: Arm64Register::LR,
            rn: Arm64Register::SP,
            offset: 16,
        });
        self.buffer.emit(Arm64Instruction::Ret);
    }

    // -- Bytecode compilation -----------------------------------------------

    /// Compile a JVM bytecode method to ARM64 instructions.
    ///
    /// `method_info` maps constant pool indices (from invokestatic operands) to
    /// the number of arguments the target method expects.
    pub fn compile_method(
        &mut self,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        bytecode: &[u8],
    ) -> Arm64CompileResult {
        self.compile_method_with_info(num_locals, num_params, max_stack, bytecode, HashMap::new())
    }

    /// Like [`compile_method`] but accepts an explicit method-info map for invoke
    /// resolution.
    ///
    /// Runs [`Arm64Backend::compile_pass`] **twice**. Branch targets are
    /// discovered as each branch is decoded, which is too late for a BACKWARD
    /// branch: its target was walked past before the label existed. Pass 1
    /// discovers every target (and every target's operand-stack shape, and
    /// every loop header); pass 2 re-walks with them in hand.
    pub fn compile_method_with_info(
        &mut self,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        bytecode: &[u8],
        method_info: HashMap<u16, usize>,
    ) -> Arm64CompileResult {
        // Discovered by pass 1 and read by pass 2, so `compile_pass` must not
        // clear them; cleared here, per compile.
        self.back_edge_targets.clear();
        self.label_states.clear();

        drop(self.compile_pass(
            num_locals,
            num_params,
            max_stack,
            bytecode,
            method_info.clone(),
            &[],
        ));
        let mut branch_targets: Vec<usize> = self.pc_labels.keys().copied().collect();
        branch_targets.sort_unstable();
        // `failed` is sticky and intentionally NOT cleared between the passes.

        self.compile_pass(
            num_locals,
            num_params,
            max_stack,
            bytecode,
            method_info,
            &branch_targets,
        )
    }

    /// One walk of the bytecode. See [`compile_method_with_info`] for why this
    /// runs twice and what `branch_targets` carries between the passes.
    pub(in crate::aarch64_backend) fn compile_pass(
        &mut self,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        bytecode: &[u8],
        method_info: HashMap<u16, usize>,
        branch_targets: &[usize],
    ) -> Arm64CompileResult {
        // Reset state.
        self.buffer = Arm64CodeBuffer::new();
        self.operand_stack.clear();
        self.held.clear();
        self.reachable = true;
        self.pc_labels.clear();
        self.cur_bytecode_pc = 0;
        self.local_regs.clear();
        self.float_local_regs.clear();
        self.num_params = num_params;
        self.method_info = method_info;
        self.stack_kinds = Self::analyze_stack_kinds(
            bytecode,
            &self.static_fields,
            &self.instance_fields,
            &self.invoke_sites,
        );
        // THE CONTEXT DECISION, made from the bytecode alone and made here.
        //
        // It has to be identical in both passes and it has to be known before
        // the frame is sized, because the context occupies a frame word and
        // shifts every argument register by one. Deriving it from the bytecode
        // -- "does this method contain a `getfield`?" -- gives both for free.
        // Deriving it from what the walk actually lowered would not: pass 1
        // and pass 2 could disagree, and the frame would already be built.
        //
        // A `getfield` that then REFUSES costs an unused frame word in a
        // method that is discarded anyway, which is the harmless direction.
        // A call that can SAFEPOINT: the three allocations (round 9 wave 18)
        // and the two REFERENCE array element accesses (wave 19), whose
        // helpers read and write a slot whose encoding only they know.
        // A CALL is a safepoint too (round 9 wave 22). `allocates` is read
        // for one thing -- whether this frame can be stopped inside a callee
        // that moves objects -- and an `invoke*` can be stopped inside
        // anything at all. The name is now narrower than the question it
        // answers; `wants_safepoint_frame` is the question.
        // ...and a type check, which resolves its target on demand and
        // allocates a `java/lang/Class` mirror doing it (round 9 wave 23).
        //
        // h23: ...and a MONITOR op, which is the most safepoint-capable call
        // on this backend -- `jit_monitor_enter`'s contended path PARKS this
        // thread, so the frame can sit inside it for an entire collection.
        // Without this term `wants_safepoint_frame` answers `false` for a
        // body whose only helper call is the lock, no safepoint-id word is
        // reserved, and `emit_helper_call_at_safepoint` refuses the method --
        // which is how `emit_monitor_op` first failed. A BYTE scan is enough
        // and errs the right way: a `0xc2`/`0xc3` that is some other
        // instruction's operand only reserves frame words nothing reads,
        // where MISSING one refuses a method that should have compiled.
        let has_monitor_op = bytecode.iter().any(|&b| b == 0xc2 || b == 0xc3);
        // h23c: `multianewarray_n` takes the VM pointer and can collect, so a
        // `0xc5` needs both the context word and a safepoint frame. Same byte
        // scan, erring the same (safe) way as the monitor one above.
        let has_multianewarray = bytecode.contains(&0xc5);
        self.calls = !invoke_sites(bytecode).is_empty()
            || !typecheck_sites(bytecode).is_empty()
            || has_monitor_op
            || has_multianewarray;
        self.allocates = !allocation_sites(bytecode).is_empty()
            || any_opcode(bytecode, |op| matches!(op, 0x32 | 0x53));
        // ...and the VM context, which every one of those helpers takes, as
        // does `getfield`, `putstatic`, a REFERENCE `putfield` and both
        // monitor ops. The reference `putfield` is the only one that cannot
        // be read off the opcode: the resolved site table answers it, and it
        // is fixed before either pass runs, so both passes decide the same
        // way.
        let stores_a_reference_field = instance_field_sites(bytecode).iter().any(|&(pc, op, _)| {
            op == 0xb5
                && self
                    .instance_fields
                    .get(&pc)
                    .is_some_and(|f| matches!(f.type_tag, b'L' | b'['))
        });
        self.needs_context = self.allocates
            // Every `invoke*` passes the VM pointer to `jit_invoke_dispatch`.
            || self.calls
            || stores_a_reference_field
            || has_monitor_op
            || has_multianewarray
            || instance_field_sites(bytecode)
                .iter()
                .any(|&(_, op, _)| op == 0xb4)
            || static_field_sites(bytecode)
                .iter()
                .any(|&(_, op, _)| op == 0xb3);
        self.static_init_classes.clear();
        // Label ids of the PREVIOUS pass's buffer; this pass's buffer was
        // just replaced, so they would name nothing (or something else).
        // `npe_throw_labels` carried this same gap before it gained a bci key
        // (round 9 wave 11 cleared only the arithmetic stub's label) -- worth
        // closing now rather than leaving more labels able to go stale.
        self.arith_throw_labels.clear();
        self.npe_throw_labels.clear();
        self.pending_local_oop_slots.clear();
        self.pending_operand_oop_slots.clear();
        self.safepoint_count = 0;
        self.incomplete_oop_maps = 0;
        self.pending_map_incomplete = false;
        // The same "must be oop" local dataflow x64 uses, seeded with this
        // method's reference parameters.
        let (lo_masks, lo_reached) = crate::x64::compute_local_oop_masks(
            bytecode,
            bytecode.len(),
            num_locals,
            self.param_oop_mask,
        );
        self.local_oop_masks = lo_masks;
        self.local_oop_reached = lo_reached;

        // The JVM local slot of each incoming argument.
        let arg_slots: Vec<usize> = match &self.param_jvm_slots {
            Some(slots) => slots.clone(),
            None => (0..num_params).collect(),
        };

        // Graph-coloring register allocation, told where the parameters really
        // are: its liveness seeds them live-on-entry, and seeding the identity
        // layout left a parameter after a `long`/`double` dead on entry, free to
        // share a register with a live one.
        let alloc = crate::regalloc::allocate_registers_arm64_with_param_slots(
            bytecode,
            bytecode.len(),
            num_locals,
            num_params,
            &arg_slots,
            &[],
        );

        for &a in &alloc.assignments {
            self.local_regs.push(a.map(Arm64Register));
        }
        while self.local_regs.len() < num_locals {
            self.local_regs.push(None);
        }

        // Float/double locals get NO dedicated FP register on this backend.
        // `regalloc::ARM64_LOCAL_FPS` is D8-D15, which AAPCS64 makes callee-saved,
        // and this prologue saves only GPRs -- homing a float local there
        // destroyed the caller's copy (aarch64 parity audit, 2026-08-01). They
        // live in frame slots. If FP homing is wanted back, the prerequisite is
        // an FP save area in `Arm64FrameLayout::compute` driven by
        // `alloc.used_xmm_regs`. Asserted by
        // `float_locals_never_use_callee_saved_fp_regs`.
        for _ in 0..num_locals {
            self.float_local_regs.push(None);
        }

        let saved_regs: Vec<Arm64Register> = alloc
            .used_callee_saved
            .iter()
            .map(|&n| Arm64Register(n))
            .collect();

        // Frame words: the frame-homed locals, the operand area, one safepoint
        // home per register-homed local and the safepoint-id word (the last two
        // only when this compilation polls). The partition is stated ONCE, here,
        // and every word index below `spill_offset` is derived from it — see
        // [`Arm64SpillArea`] for why it is a type and not four additions.
        let safepoint_frame = self.wants_safepoint_frame();
        let area = Arm64SpillArea {
            locals: self.local_spill_count(),
            operands: max_stack,
            // The safepoint region is reserved for a method that POLLS or a
            // method that ALLOCATES: both stop this frame inside a callee that
            // can move objects, and both therefore need somewhere to put the
            // register-homed reference locals and an id the collector can
            // select their map by.
            //
            // One word per register-homed LOCAL, which is what
            // `safepoint_home_for_reg_local` indexes by -- NOT `saved_regs.len()`,
            // which is how many distinct callee-saved REGISTERS the allocator
            // used. The two differ whenever two locals with disjoint live
            // ranges share one register, and then the second local's `k` runs
            // off the end of the region and `safepoint_home_word` answers
            // `None`: the poll (or, since wave 18, the allocation call)
            // refuses the method rather than home it. Found by the first test
            // to put two reference locals in one register across a call.
            safepoint_homes: if safepoint_frame {
                self.local_regs.iter().filter(|r| r.is_some()).count()
            } else {
                0
            },
            sp_id: usize::from(safepoint_frame),
            context: usize::from(self.needs_context),
        };
        self.install_frame(num_locals, area, &saved_regs);
        // The sp-id word sits past the locals, the operand area and the homes —
        // which is what `sp_id_word()` says, rather than "the last word", which
        // is what `num_spills - 1` said and which is only the same answer while
        // the id word happens to be laid out last.
        self.sp_id_slot_off = match self.spill_word_offset(self.spill_area.sp_id_word()) {
            // The runtime reads `[frame_base - off]`, so publish the magnitude.
            Some(off) => -off,
            None => 0,
        };

        self.epilogue_label = self.buffer.new_label();

        self.emit_prologue();
        self.emit_argument_homing(&arg_slots);
        // Publish the frame base BEFORE the first poll stamps an id into it.
        self.emit_frame_record();
        // METHOD-ENTRY SAFEPOINT POLL, after the arguments are homed: its call
        // may destroy X0-X7.
        self.emit_safepoint_poll(true);

        let mut pc = 0;
        let mut success = true;
        while pc < bytecode.len() {
            // Pre-seed this PC's label if the discovery pass saw a branch to it.
            if branch_targets.binary_search(&pc).is_ok() {
                let _ = self.label_for_pc_unchecked(pc);
            }
            self.held.clear();

            if let Some(&label) = self.pc_labels.get(&pc) {
                if self.reachable {
                    self.arrive_at_target(pc);
                } else {
                    self.restore_stack_at(pc);
                }
                if !self.buffer.labels.contains_key(&label) {
                    self.buffer.bind_label(label);
                }
            } else if !self.reachable {
                self.restore_stack_at(pc);
            }
            self.reachable = true;

            let opcode = bytecode[pc];
            let start_pc = pc;

            // THE SHARED-MEMORY GATE. Consulted before the dispatch below, so
            // no lowering arm can be reached for an opcode that touches memory
            // another thread observes unless that arm orders its own accesses.
            // The refusal used to be an ACCIDENT of which arms happen to be
            // written -- invisible to anyone about to write one, and so the
            // first `getfield` arm would have lowered a `volatile` read to a
            // plain `LDR`, which reviews as correct against an x86-first
            // mental model and is wrong on every ARM core.
            //
            // `opcode_has_ordered_lowering` is the whole permission since
            // round 9 wave 14: a per-opcode list, each entry a reviewed claim
            // about ONE lowering, and each listed arm still refuses any site
            // it was handed nothing for. There is deliberately no blanket
            // constant in front of it any more -- see
            // `ARM64_LOWERS_ACQUIRE_RELEASE` for what that one was and why a
            // single `bool` was both too weak and unreachable.
            if opcode_touches_shared_memory(opcode) && !opcode_has_ordered_lowering(opcode) {
                self.buffer.emit(Arm64Instruction::Comment(format!(
                    "opcode 0x{opcode:02x} at pc={start_pc} touches shared memory, and this \
                     backend emits no LDAR/STLR/DMB — bailing to interpreter"
                )));
                success = false;
                break;
            }
            // Published before the loop-header poll (whose id and oop-local mask
            // are this pc's) and before lowering (so `label_for_pc` can tell a
            // back-edge from a forward branch).
            self.cur_bytecode_pc = start_pc;

            // LOOP-HEADER SAFEPOINT POLL, after the label so a back edge lands on
            // it -- one poll per header regardless of how many branches target it.
            if self.back_edge_targets.contains(&pc) {
                self.emit_safepoint_poll(false);
            }
            pc += 1;

            match opcode {
                // nop
                0x00 => self.buffer.emit(Arm64Instruction::Nop),
                // aconst_null
                0x01 => {
                    let dst = self.alloc_reg(false);
                    self.buffer
                        .emit(Arm64Instruction::MovImm { rd: dst, imm: 0 });
                    self.push_reg(OperandKind::Ref, dst);
                }
                // iconst_m1 .. iconst_5
                0x02..=0x08 => self.emit_iconst(i32::from(opcode) - 3),
                // lconst_0, lconst_1
                0x09 | 0x0a => self.emit_lconst(i64::from(opcode - 0x09)),
                // fconst_0 .. fconst_2
                0x0b..=0x0d => self.emit_fconst(f32::from(opcode - 0x0b)),
                // dconst_0, dconst_1
                0x0e | 0x0f => self.emit_dconst(f64::from(opcode - 0x0e)),
                // bipush
                0x10 => {
                    let Some(v) = bc_u8(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 1;
                    // Cast: bipush's operand is a signed byte.
                    self.emit_iconst(i32::from(v as i8));
                }
                // sipush
                0x11 => {
                    let Some(v) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    self.emit_iconst(i32::from(v));
                }
                // ldc / ldc_w / ldc2_w: this backend is never handed the
                // constant pool, so it cannot recover the constant. Refuse.
                0x12..=0x14 => {
                    self.buffer.emit(Arm64Instruction::Comment(
                        "ldc family: constant pool not available — bailing to interpreter".into(),
                    ));
                    success = false;
                    break;
                }
                // iload lload fload dload aload
                0x15..=0x19 => {
                    let Some(idx) = bc_u8(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 1;
                    self.emit_load_local(usize::from(idx), LOCAL_KINDS[usize::from(opcode - 0x15)]);
                }
                // iload_0 .. aload_3
                0x1a..=0x2d => {
                    let n = opcode - 0x1a;
                    self.emit_load_local(usize::from(n % 4), LOCAL_KINDS[usize::from(n / 4)]);
                }
                // istore lstore fstore dstore astore
                0x36..=0x3a => {
                    let Some(idx) = bc_u8(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 1;
                    self.emit_store_local(
                        usize::from(idx),
                        LOCAL_KINDS[usize::from(opcode - 0x36)],
                    );
                }
                // istore_0 .. astore_3
                0x3b..=0x4e => {
                    let n = opcode - 0x3b;
                    self.emit_store_local(usize::from(n % 4), LOCAL_KINDS[usize::from(n / 4)]);
                }
                // aaload (round 9 wave 19). The ONE array access that goes
                // through a helper, because a reference element is not always
                // a pointer: compressed oops make it a 4-byte scaled offset
                // and an armed ZGC cycle makes it a coloured word. Its guards
                // are still this backend's own.
                0x32 => {
                    if !self.emit_aaload() {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "array load 0x32 at pc={start_pc}: no exact array exception path, or a reference element — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // aastore (round 9 wave 19). Two calls at one bytecode: the
                // element-type check, whose verdict a compiled caller can
                // see, and the store itself with its SATB pre-barrier and
                // card mark.
                0x53 => {
                    if !self.emit_aastore() {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "array store 0x53 at pc={start_pc}: no exact array exception path, or a reference element — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // iaload .. saload, minus `aaload` above (round 9 wave 13).
                // Past the gate for the same reason `arraylength` is: the
                // lowering orders every access it makes.
                0x2e..=0x35 => {
                    if !self.emit_array_load(opcode) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "array load 0x{opcode:02x} at pc={start_pc}: no exact array exception path, or a reference element — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // iastore .. sastore, minus `aastore` above (round 9 wave 13).
                0x4f..=0x56 => {
                    if !self.emit_array_store(opcode) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "array store 0x{opcode:02x} at pc={start_pc}: no exact array exception path, or a reference element — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // pop: one category-1 value.
                0x57 => match self.top_categories(1) {
                    Some(c) if !c[0] => self.drop_top(),
                    _ => {
                        success = false;
                        break;
                    }
                },
                // pop2: one category-2 value, or two category-1 values.
                0x58 => match self.top_categories(1) {
                    Some(c) if c[0] => self.drop_top(),
                    Some(_) => match self.top_categories(2) {
                        Some(c) if !c[1] => {
                            self.drop_top();
                            self.drop_top();
                        }
                        _ => {
                            success = false;
                            break;
                        }
                    },
                    None => {
                        success = false;
                        break;
                    }
                },
                // dup: category-1 top.
                0x59 => match self.top_categories(1) {
                    Some(c) if !c[0] => self.emit_dup_group_over(1, 0),
                    _ => {
                        success = false;
                        break;
                    }
                },
                // dup_x1: both category-1.
                0x5a => match self.top_categories(2) {
                    Some(c) if !c[0] && !c[1] => self.emit_dup_group_over(1, 1),
                    _ => {
                        success = false;
                        break;
                    }
                },
                // dup_x2: FORM 1 is three category-1 values; FORM 2 a category-1
                // top over one category-2 value.
                0x5b => {
                    let under = match self.top_categories(2) {
                        Some(c) if !c[0] && c[1] => Some(1),
                        Some(c) if !c[0] => match self.top_categories(3) {
                            Some(c3) if !c3[2] => Some(2),
                            _ => None,
                        },
                        _ => None,
                    };
                    let Some(under) = under else {
                        success = false;
                        break;
                    };
                    self.emit_dup_group_over(1, under);                }
                // dup2: FORM 1 two category-1 values; FORM 2 one category-2.
                0x5c => {
                    let group = match self.top_categories(1) {
                        Some(c) if c[0] => Some(1),
                        Some(_) => match self.top_categories(2) {
                            Some(c) if !c[1] => Some(2),
                            _ => None,
                        },
                        None => None,
                    };
                    let Some(group) = group else {
                        success = false;
                        break;
                    };
                    self.emit_dup_group_over(group, 0);                }
                // dup2_x1: FORM 1 two category-1 over one; FORM 2 one category-2
                // over one. The entry underneath is category-1 in both.
                0x5d => {
                    let group = match self.top_categories(2) {
                        Some(c) if c[0] && !c[1] => Some(1),
                        Some(c) if !c[0] && !c[1] => match self.top_categories(3) {
                            Some(c3) if !c3[2] => Some(2),
                            _ => None,
                        },
                        _ => None,
                    };
                    let Some(group) = group else {
                        success = false;
                        break;
                    };
                    self.emit_dup_group_over(group, 1);                }
                // dup2_x2, in this backend's one-entry-per-value model:
                //   FORM 4  v1,v2 cat-2   [v2, v1]         -> [v1, v2, v1]
                //   FORM 2  v1 cat-2      [v3, v2, v1]     -> [v1, v3, v2, v1]
                //   FORM 3  v3 cat-2      [v3, v2, v1]     -> [v2, v1, v3, v2, v1]
                //   FORM 1  all cat-1     [v4, v3, v2, v1] -> [v2, v1, v4, v3, v2, v1]
                0x5e => {
                    let shape = match self.top_categories(2) {
                        Some(c) if c[0] && c[1] => Some((1usize, 1usize)),
                        Some(c) if c[0] => match self.top_categories(3) {
                            Some(c3) if !c3[2] => Some((1, 2)),
                            _ => None,
                        },
                        Some(c) if !c[1] => match self.top_categories(3) {
                            Some(c3) if c3[2] => Some((2, 1)),
                            Some(_) => match self.top_categories(4) {
                                Some(c4) if !c4[3] => Some((2, 2)),
                                _ => None,
                            },
                            None => None,
                        },
                        _ => None,
                    };
                    let Some((group, under)) = shape else {
                        success = false;
                        break;
                    };
                    self.emit_dup_group_over(group, under);                }
                // swap: both category-1.
                0x5f => match self.top_categories(2) {
                    Some(c) if !c[0] && !c[1] => {
                        let (Some(v1), Some(v2)) = (self.pop_entry(), self.pop_entry()) else {
                            success = false;
                            break;
                        };
                        self.push_like(v1.1, v1.0);
                        self.push_like(v2.1, v2.0);
                    }
                    _ => {
                        success = false;
                        break;
                    }
                },
                // iadd ladd fadd dadd
                0x60 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::AddW { rd, rn, rm }
                }),
                0x61 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Add { rd, rn, rm }
                }),
                0x62 | 0x63 => self.emit_binary_fp(
                    if opcode == 0x62 {
                        OperandKind::F32
                    } else {
                        OperandKind::F64
                    },
                    |vd, vn, vm| Arm64Instruction::FaddSingle { vd, vn, vm },
                    |vd, vn, vm| Arm64Instruction::FaddDouble { vd, vn, vm },
                ),
                // isub lsub fsub dsub
                0x64 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::SubW { rd, rn, rm }
                }),
                0x65 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Sub { rd, rn, rm }
                }),
                0x66 | 0x67 => self.emit_binary_fp(
                    if opcode == 0x66 {
                        OperandKind::F32
                    } else {
                        OperandKind::F64
                    },
                    |vd, vn, vm| Arm64Instruction::FsubSingle { vd, vn, vm },
                    |vd, vn, vm| Arm64Instruction::FsubDouble { vd, vn, vm },
                ),
                // imul lmul fmul dmul
                0x68 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::MulW { rd, rn, rm }
                }),
                0x69 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Mul { rd, rn, rm }
                }),
                0x6a | 0x6b => self.emit_binary_fp(
                    if opcode == 0x6a {
                        OperandKind::F32
                    } else {
                        OperandKind::F64
                    },
                    |vd, vn, vm| Arm64Instruction::FmulSingle { vd, vn, vm },
                    |vd, vn, vm| Arm64Instruction::FmulDouble { vd, vn, vm },
                ),
                // idiv / ldiv / irem / lrem. Round 9 wave 11: lowered with a
                // zero-divisor guard that branches to the shared
                // `ArithmeticException` stub (`emit_int_div_rem`,
                // `emit_arith_throw_stub`) -- but ONLY when that throw path is
                // wired and exact (`can_throw_arithmetic`: the helper address is
                // set and the caller vouched the exception table is empty).
                // Otherwise refused, as since round 7: the old guard was
                // `BRK #1` (SIGTRAP, which nothing converts), and AArch64 `SDIV`
                // by zero yields 0, so `x % 0` silently returned `x`.
                0x6c | 0x6d | 0x70 | 0x71 => {
                    if !self.can_throw_arithmetic() {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "opcode 0x{opcode:02x} at pc={start_pc}: no exact \
                             ArithmeticException path (helper unwired or exception \
                             table not known empty) — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                    let kind = if opcode == 0x6c || opcode == 0x70 {
                        OperandKind::I32
                    } else {
                        OperandKind::I64
                    };
                    self.emit_int_div_rem(kind, opcode >= 0x70);
                }
                // fdiv ddiv
                0x6e | 0x6f => self.emit_binary_fp(
                    if opcode == 0x6e {
                        OperandKind::F32
                    } else {
                        OperandKind::F64
                    },
                    |vd, vn, vm| Arm64Instruction::FdivSingle { vd, vn, vm },
                    |vd, vn, vm| Arm64Instruction::FdivDouble { vd, vn, vm },
                ),
                // frem / drem -- refused: there is no exact lowering (the
                // truncating FCVTZS round-trip saturates for |a/b| >= 2^63, and
                // Java's remainder is exact across the whole range).
                0x72 | 0x73 => {
                    self.buffer.emit(Arm64Instruction::Comment(
                        "frem/drem: no exact lowering — bailing to interpreter".into(),
                    ));
                    success = false;
                    break;
                }
                // ineg lneg fneg dneg
                0x74 => {
                    let src = self.pop_kind(OperandKind::I32);
                    let dst = self.alloc_reg(false);
                    self.buffer
                        .emit(Arm64Instruction::NegW { rd: dst, rn: src });
                    self.emit_sxtw(dst);
                    self.push_reg(OperandKind::I32, dst);
                }
                0x75 => {
                    let src = self.pop_kind(OperandKind::I64);
                    let dst = self.alloc_reg(false);
                    self.buffer.emit(Arm64Instruction::Neg { rd: dst, rn: src });
                    self.push_reg(OperandKind::I64, dst);
                }
                0x76 => self.emit_float_neg(OperandKind::F32),
                0x77 => self.emit_float_neg(OperandKind::F64),
                // ishl lshl ishr lshr iushr lushr. The W-form variable shifts take
                // the amount MOD 32 and the X forms MOD 64, which is exactly the
                // JVMS `& 0x1f` / `& 0x3f`.
                0x78 => self.emit_shift(OperandKind::I32, |rd, rn, rm| Arm64Instruction::LslW {
                    rd,
                    rn,
                    rm,
                }),
                0x79 => self.emit_shift(OperandKind::I64, |rd, rn, rm| Arm64Instruction::Lsl {
                    rd,
                    rn,
                    rm,
                }),
                0x7a => self.emit_shift(OperandKind::I32, |rd, rn, rm| Arm64Instruction::AsrW {
                    rd,
                    rn,
                    rm,
                }),
                0x7b => self.emit_shift(OperandKind::I64, |rd, rn, rm| Arm64Instruction::Asr {
                    rd,
                    rn,
                    rm,
                }),
                0x7c => self.emit_shift(OperandKind::I32, |rd, rn, rm| Arm64Instruction::LsrW {
                    rd,
                    rn,
                    rm,
                }),
                0x7d => self.emit_shift(OperandKind::I64, |rd, rn, rm| Arm64Instruction::Lsr {
                    rd,
                    rn,
                    rm,
                }),
                // iand land ior lor ixor lxor
                0x7e => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::AndW { rd, rn, rm }
                }),
                0x7f => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::And { rd, rn, rm }
                }),
                0x80 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::OrrW { rd, rn, rm }
                }),
                0x81 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Orr { rd, rn, rm }
                }),
                0x82 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::EorW { rd, rn, rm }
                }),
                0x83 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Eor { rd, rn, rm }
                }),
                // iinc
                0x84 => {
                    let (Some(idx), Some(delta)) = (bc_u8(bytecode, pc), bc_u8(bytecode, pc + 1))
                    else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    // Cast: iinc's constant is a signed byte.
                    self.emit_iinc(usize::from(idx), i32::from(delta as i8));
                }
                // i2l l2i i2f i2d l2f l2d f2i f2l f2d d2i d2l d2f i2b i2c i2s
                0x85..=0x93 => self.emit_conversion(opcode),
                // lcmp
                0x94 => self.emit_lcmp(),
                // fcmpl fcmpg dcmpl dcmpg
                0x95 => self.emit_fcmp(OperandKind::F32, false),
                0x96 => self.emit_fcmp(OperandKind::F32, true),
                0x97 => self.emit_fcmp(OperandKind::F64, false),
                0x98 => self.emit_fcmp(OperandKind::F64, true),
                // ifeq ifne iflt ifge ifgt ifle
                0x99..=0x9e => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    let cond = IF_CONDS[usize::from(opcode - 0x99)];
                    self.emit_if_zero(cond, branch_target(start_pc, i32::from(off)));
                }
                // if_icmpeq .. if_icmple
                0x9f..=0xa4 => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    let cond = IF_CONDS[usize::from(opcode - 0x9f)];
                    self.emit_if_icmp(cond, branch_target(start_pc, i32::from(off)));
                }
                // if_acmpeq if_acmpne
                0xa5 | 0xa6 => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    let cond = if opcode == 0xa5 {
                        Arm64Condition::Eq
                    } else {
                        Arm64Condition::Ne
                    };
                    self.emit_if_acmp(cond, branch_target(start_pc, i32::from(off)));
                }
                // goto
                0xa7 => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    let label = self.prepare_branch(branch_target(start_pc, i32::from(off)));
                    self.buffer.emit(Arm64Instruction::B { label });
                    self.reachable = false;
                }
                // tableswitch. The `checked_tableswitch_count` audit rejects
                // adversarial overflow / oversize tables, as on x64.
                0xaa => {
                    let key = self.pop_kind(OperandKind::I32);
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    let (Some(default_off), Some(low), Some(high)) = (
                        bc_i32(bytecode, pc),
                        bc_i32(bytecode, pc + 4),
                        bc_i32(bytecode, pc + 8),
                    ) else {
                        success = false;
                        break;
                    };
                    pc += 12;
                    let Some(count) = crate::x64::checked_tableswitch_count(low, high) else {
                        success = false;
                        break;
                    };
                    let default = self.prepare_branch(branch_target(start_pc, default_off));
                    let mut targets = Vec::with_capacity(count);
                    for _ in 0..count {
                        let Some(off) = bc_i32(bytecode, pc) else {
                            break;
                        };
                        pc += 4;
                        targets.push(self.prepare_branch(branch_target(start_pc, off)));
                    }
                    if targets.len() != count {
                        success = false;
                        break;
                    }
                    self.buffer.emit(Arm64Instruction::TableSwitch {
                        key,
                        low,
                        default,
                        targets,
                    });
                    self.reachable = false;
                }
                // lookupswitch, with `checked_lookupswitch_npairs` validation.
                0xab => {
                    let key = self.pop_kind(OperandKind::I32);
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    let (Some(default_off), Some(npairs_raw)) =
                        (bc_i32(bytecode, pc), bc_i32(bytecode, pc + 4))
                    else {
                        success = false;
                        break;
                    };
                    pc += 8;
                    let Some(npairs) = crate::x64::checked_lookupswitch_npairs(npairs_raw) else {
                        success = false;
                        break;
                    };
                    let default = self.prepare_branch(branch_target(start_pc, default_off));
                    let mut pairs = Vec::with_capacity(npairs);
                    for _ in 0..npairs {
                        let (Some(value), Some(off)) =
                            (bc_i32(bytecode, pc), bc_i32(bytecode, pc + 4))
                        else {
                            break;
                        };
                        pc += 8;
                        pairs.push((value, self.prepare_branch(branch_target(start_pc, off))));
                    }
                    if pairs.len() != npairs {
                        success = false;
                        break;
                    }
                    self.buffer.emit(Arm64Instruction::LookupSwitch {
                        key,
                        pairs,
                        default,
                    });
                    self.reachable = false;
                }
                // ireturn lreturn freturn dreturn areturn
                0xac..=0xb0 => {
                    const RETURN_KINDS: [OperandKind; 5] = LOCAL_KINDS;
                    self.emit_return_value(RETURN_KINDS[usize::from(opcode - 0xac)]);
                }
                // return
                0xb1 => {
                    self.buffer.emit(Arm64Instruction::B {
                        label: self.epilogue_label,
                    });
                    self.reachable = false;
                }
                // getstatic (round 9 wave 10). The one shared-memory opcode
                // `opcode_has_ordered_lowering` lets past the gate; it lowers
                // only a site the caller resolved (`set_static_field_info`)
                // to a primitive static, and refuses everything else here.
                0xb2 => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    pc += 2;
                    let Some(field) = self.static_fields.get(&start_pc).copied() else {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "getstatic at pc={start_pc}: no resolved static field — \
                             bailing to interpreter"
                        )));
                        success = false;
                        break;
                    };
                    if !self.emit_getstatic(field) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "getstatic at pc={start_pc}: type '{}' has no aarch64 \
                             lowering — bailing to interpreter",
                            char::from(field.type_tag)
                        )));
                        success = false;
                        break;
                    }
                }
                // athrow (round 9 wave 20). Past the gate because it
                // publishes nothing: the exception object was published by
                // whatever created it, and this hands the reference to a
                // helper and leaves.
                0xbf => {
                    if !self.emit_athrow() {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "athrow at pc={start_pc}: an unwired helper, or a non-empty exception table — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // new / newarray / anewarray (round 9 wave 18). Past the
                // gate because their lowering owes no fence and says why: the
                // object is unreachable by construction until a later store
                // publishes it, and that store carries its own ordering.
                // Refused when the site was not resolved, the helper is
                // unwired, or this method has an exception table (the OOM the
                // helper stashes is drained with an unknown pc).
                0xbb | 0xbd => {
                    let Some(operand) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    // Cast: a constant-pool index, which is unsigned.
                    if !self.emit_allocation(opcode, operand as u16) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "allocation at pc={start_pc}: no resolved class, an unwired helper, or a non-empty exception table — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                0xbc => {
                    let Some(atype) = bc_u8(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 1;
                    if !self.emit_allocation(opcode, u16::from(atype)) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "allocation at pc={start_pc}: no resolved class, an unwired helper, or a non-empty exception table — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // putstatic (round 9 wave 17). Past the gate because its
                // lowering orders its own access: a volatile store gets a
                // `DMB ISH` on both sides, the store itself happening inside a
                // helper that takes the statics write lock. Refused when the
                // site was not resolved (which is also the proof that the
                // declaring class is initialized), when the field is a
                // reference, or when the helper is unwired.
                0xb3 => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    pc += 2;
                    let Some(field) = self.static_fields.get(&start_pc).copied() else {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "putstatic at pc={start_pc}: no resolved static field — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    };
                    if !self.emit_putstatic(field) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "putstatic at pc={start_pc}: an unwired helper, an uninitialized class, or a reference static — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // getfield (round 9 wave 16). Past the gate because its
                // lowering orders its own access: the read happens inside
                // `jit_getfield`, which takes the object's own lock-free path,
                // and a volatile instance READ owes no fence this backend can
                // place around a helper -- see `emit_getfield`. Refused when
                // the site was not resolved, when the field is a reference, or
                // when the helper, the context word or the exact NPE path is
                // missing.
                0xb4 => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    pc += 2;
                    let Some(field) = self.instance_fields.get(&start_pc).copied() else {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "getfield at pc={start_pc}: no resolved instance field — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    };
                    if !self.emit_getfield(field) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "getfield at pc={start_pc}: no exact NullPointerException path, an unwired helper, or an unrecognised type tag — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // putfield (round 9 wave 15). Past the gate because its
                // lowering orders its own access -- a volatile store gets a
                // `DMB ISH` on both sides, because the store itself happens
                // inside a helper where it is relaxed. Refused here when the
                // site was not resolved, when the field is a reference, or
                // when the NPE path a null receiver needs is not exact.
                0xb5 => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    pc += 2;
                    let Some(field) = self.instance_fields.get(&start_pc).copied() else {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "putfield at pc={start_pc}: no resolved instance field — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    };
                    if !self.emit_putfield(field) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "putfield at pc={start_pc}: no exact NullPointerException path, an unwired helper, or a reference field — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // arraylength (round 9 wave 12). Past the gate for the same
                // reason `getstatic` is -- its lowering orders every access it
                // makes -- and refused here when the NPE throw path it needs
                // for a null receiver is not wired or not exact.
                0xbe => {
                    if !self.emit_arraylength() {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "arraylength at pc={start_pc}: no exact NullPointerException path — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // multianewarray (h23c, 2026-09-22).
                0xc5 => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    let Some(&ndims) = bytecode.get(pc + 2) else {
                        success = false;
                        break;
                    };
                    pc += 3;
                    if !self.emit_multianewarray(usize::from(ndims)) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "multianewarray at pc={start_pc}: no resolved site, an unwired helper, an unsupported arity, or no route for a pending exception — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // monitorenter / monitorexit (h23, 2026-09-22).
                0xc2 | 0xc3 => {
                    if !self.emit_monitor_op(opcode == 0xc2) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "monitor at pc={start_pc}: an unwired helper, or no route for a pending exception — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // checkcast / instanceof (round 9 wave 23).
                0xc0 | 0xc1 => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    pc += 2;
                    if !self.emit_typecheck(opcode) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "typecheck at pc={start_pc}: unresolved target class, an unwired helper, or a non-empty exception table — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // THE FIVE INVOKE FORMS (round 9 wave 22), all through
                // `jit_invoke_dispatch`. The operand consumed by the
                // instruction differs in WIDTH, not in meaning: `0xb9`
                // carries a count byte and a zero after its index and `0xba`
                // two zeros, and reading three bytes for either walks into the
                // next instruction.
                0xb6..=0xba => {
                    if bc_i16(bytecode, pc).is_none() {
                        success = false;
                        break;
                    }
                    pc += if matches!(opcode, 0xb9 | 0xba) { 4 } else { 2 };
                    if !self.emit_invoke(opcode) {
                        self.buffer.emit(Arm64Instruction::Comment(format!(
                            "invoke at pc={start_pc}: unresolved site, unwired dispatch helper or a non-empty exception table — bailing to interpreter"
                        )));
                        success = false;
                        break;
                    }
                }
                // ifnull ifnonnull
                0xc6 | 0xc7 => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    self.emit_if_null(opcode == 0xc7, branch_target(start_pc, i32::from(off)));
                }
                _ => {
                    self.buffer.emit(Arm64Instruction::Comment(format!(
                        "unsupported opcode 0x{:02x} at pc={}",
                        opcode, start_pc
                    )));
                    success = false;
                    break;
                }
            }
        }

        self.emit_epilogue();
        // Out of line, after the `RET`: straight-line code never falls into
        // these.
        self.emit_arith_throw_stub();
        self.emit_npe_throw_stubs();
        self.emit_aioobe_stubs();

        let frame = match self.frame.take() {
            Some(f) => f,
            None => {
                return Arm64CompileResult {
                    instructions: Vec::new(),
                    frame: Arm64FrameLayout {
                        frame_size: 0,
                        callee_save_offset: 0,
                        spill_offset: 0,
                        num_spills: 0,
                        saved_regs: Vec::new(),
                        num_reg_locals: 0,
                    },
                    labels: HashMap::new(),
                    success: false,
                    pending_oop_maps: Vec::new(),
                    sp_id_slot_off: 0,
                    safepoint_count: 0,
                    incomplete_oop_maps: 0,
                    static_init_classes: Vec::new(),
                    needs_context: false,
                    polls_enabled: false,
                    frame_base_published: false,
                }
            }
        };
        let mut static_init_classes = std::mem::take(&mut self.static_init_classes);
        static_init_classes.sort_unstable();
        static_init_classes.dedup();
        Arm64CompileResult {
            static_init_classes,
            needs_context: self.needs_context,
            polls_enabled: self.safepoints_enabled,
            frame_base_published: self.frame_base_published,
            instructions: self.buffer.instructions.clone(),
            frame,
            labels: self.buffer.labels.clone(),
            success: success && !self.failed,
            sp_id_slot_off: self.sp_id_slot_off,
            safepoint_count: self.safepoint_count,
            incomplete_oop_maps: self.incomplete_oop_maps,
            pending_oop_maps: std::mem::take(&mut self.pending_oop_maps),
        }
    }

    // -- Arithmetic ----------------------------------------------------------

    /// A two-operand integer operation on `kind` operands. For `I32` the
    /// instruction is a W form, whose result is the JVMS 32-bit wrapped value,
    /// and the result is sign-extended back to the canonical form.
    pub(in crate::aarch64_backend) fn emit_binary_int(
        &mut self,
        kind: OperandKind,
        make: fn(Arm64Register, Arm64Register, Arm64Register) -> Arm64Instruction,
    ) {
        let rhs = self.pop_kind(kind);
        let lhs = self.pop_kind(kind);
        let dst = self.alloc_reg(false);
        self.buffer.emit(make(dst, lhs, rhs));
        if kind == OperandKind::I32 {
            self.emit_sxtw(dst);
        }
        self.push_reg(kind, dst);
    }

    /// A shift of a `kind` value by an `int` amount.
    pub(in crate::aarch64_backend) fn emit_shift(
        &mut self,
        kind: OperandKind,
        make: fn(Arm64Register, Arm64Register, Arm64Register) -> Arm64Instruction,
    ) {
        let amount = self.pop_kind(OperandKind::I32);
        let value = self.pop_kind(kind);
        let dst = self.alloc_reg(false);
        self.buffer.emit(make(dst, value, amount));
        if kind == OperandKind::I32 {
            self.emit_sxtw(dst);
        }
        self.push_reg(kind, dst);
    }

    /// A two-operand FP operation, in the S form for `float` and the D form
    /// for `double`. `float` used to be computed as `double` end to end, which
    /// gets rounding, overflow and the bits handed back to the VM all wrong.
    pub(in crate::aarch64_backend) fn emit_binary_fp(
        &mut self,
        kind: OperandKind,
        single: fn(Arm64Register, Arm64Register, Arm64Register) -> Arm64Instruction,
        double: fn(Arm64Register, Arm64Register, Arm64Register) -> Arm64Instruction,
    ) {
        let rhs = self.pop_kind(kind);
        let lhs = self.pop_kind(kind);
        let dst = self.alloc_reg(true);
        let inst = if kind == OperandKind::F32 {
            single(dst, lhs, rhs)
        } else {
            double(dst, lhs, rhs)
        };
        self.buffer.emit(inst);
        self.push_reg(kind, dst);
    }

    /// `fneg` / `dneg`: FNEG, which flips the sign bit. The old `0.0 - x`
    /// turned `-(+0.0)` into `+0.0` rather than `-0.0`.
    pub(in crate::aarch64_backend) fn emit_float_neg(&mut self, kind: OperandKind) {
        let src = self.pop_kind(kind);
        let dst = self.alloc_reg(true);
        let inst = if kind == OperandKind::F32 {
            Arm64Instruction::FnegSingle { vd: dst, vn: src }
        } else {
            Arm64Instruction::FnegDouble { vd: dst, vn: src }
        };
        self.buffer.emit(inst);
        self.push_reg(kind, dst);
    }

    /// `iinc`: a W-form add, re-sign-extended, on the local's home.
    pub(in crate::aarch64_backend) fn emit_iinc(&mut self, index: usize, delta: i32) {
        let Some(home) = self.local_regs.get(index).copied() else {
            self.failed = true;
            return;
        };
        let add = |rd: Arm64Register| {
            if delta >= 0 {
                Arm64Instruction::AddImmW {
                    rd,
                    rn: rd,
                    imm: delta,
                }
            } else {
                Arm64Instruction::SubImmW {
                    rd,
                    rn: rd,
                    imm: -delta,
                }
            }
        };
        match home {
            Some(reg) => {
                self.buffer.emit(add(reg));
                self.emit_sxtw(reg);
            }
            None => {
                let Some(offset) = self.local_slot_offset(index) else {
                    self.failed = true;
                    return;
                };
                let tmp = self.alloc_reg(false);
                self.buffer.emit(Arm64Instruction::Ldr {
                    rt: tmp,
                    rn: Arm64Register::FP,
                    offset,
                });
                self.buffer.emit(add(tmp));
                self.emit_sxtw(tmp);
                self.buffer.emit(Arm64Instruction::Str {
                    rt: tmp,
                    rn: Arm64Register::FP,
                    offset,
                });
            }
        }
    }

    /// The numeric conversions `i2l` (0x85) through `i2s` (0x93).
    ///
    /// Width discipline, given that an `int` is held sign-extended:
    /// * `i2l` is a relabelling -- the register already holds the `long`.
    /// * `l2i` is `SXTW`, keeping the low 32 bits as a signed value. It used to
    ///   `AND` with `0xFFFFFFFF`, i.e. zero-extend, so `(int) -1L` read back as
    ///   4294967295 anywhere the upper half was observed.
    /// * `f2i`/`d2i` are `FCVTZS W` then `SXTW`: the W form saturates at the
    ///   32-bit bounds, as the JVMS requires. The X form they used saturated at
    ///   the 64-bit bounds, so `(int) 1e10` was not `Integer.MAX_VALUE`.
    /// * `i2f`/`i2d` read the W register; `l2f`/`l2d` read the X register.
    pub(in crate::aarch64_backend) fn emit_conversion(&mut self, opcode: u8) {
        use OperandKind::{F32, F64, I32, I64};
        match opcode {
            // i2l
            0x85 => {
                let Some(top) = self.operand_stack.last_mut() else {
                    self.failed = true;
                    return;
                };
                if top.kind != I32 {
                    self.failed = true;
                    return;
                }
                top.kind = I64;
            }
            0x86 => self.emit_convert(I32, F32, false, |d, s| Arm64Instruction::ScvtfSingle {
                vd: d,
                rn: s,
            }),
            0x87 => self.emit_convert(I32, F64, false, |d, s| Arm64Instruction::ScvtfDoubleW {
                vd: d,
                rn: s,
            }),
            0x88 => self.emit_convert(I64, I32, false, |d, s| Arm64Instruction::Sxtw {
                rd: d,
                rn: s,
            }),
            0x89 => self.emit_convert(I64, F32, false, |d, s| Arm64Instruction::ScvtfSingleX {
                vd: d,
                rn: s,
            }),
            0x8a => self.emit_convert(I64, F64, false, |d, s| Arm64Instruction::ScvtfDouble {
                vd: d,
                rn: s,
            }),
            0x8b => self.emit_convert(F32, I32, true, |d, s| Arm64Instruction::FcvtzsSingle {
                rd: d,
                vn: s,
            }),
            0x8c => self.emit_convert(F32, I64, false, |d, s| Arm64Instruction::FcvtzsSingleX {
                rd: d,
                vn: s,
            }),
            0x8d => self.emit_convert(F32, F64, false, |d, s| {
                Arm64Instruction::FcvtSingleToDouble { vd: d, vn: s }
            }),
            0x8e => self.emit_convert(F64, I32, true, |d, s| Arm64Instruction::FcvtzsIntW {
                rd: d,
                vn: s,
            }),
            0x8f => self.emit_convert(F64, I64, false, |d, s| Arm64Instruction::FcvtzsInt {
                rd: d,
                vn: s,
            }),
            0x90 => self.emit_convert(F64, F32, false, |d, s| {
                Arm64Instruction::FcvtDoubleToSingle { vd: d, vn: s }
            }),
            // i2b, i2c (zero-extend 16 bits), i2s
            0x91 => self.emit_convert(I32, I32, false, |d, s| Arm64Instruction::Sxtb {
                rd: d,
                rn: s,
            }),
            0x92 => self.emit_convert(I32, I32, false, |d, s| Arm64Instruction::AndImm {
                rd: d,
                rn: s,
                imm: 0xFFFF,
            }),
            0x93 => self.emit_convert(I32, I32, false, |d, s| Arm64Instruction::Sxth {
                rd: d,
                rn: s,
            }),
            _ => self.failed = true,        }    }

    /// Pop a `from`, emit `make(dst, src)` into a fresh register of the `to`
    /// class, sign-extend when `sxtw`, and push a `to`.
    pub(in crate::aarch64_backend) fn emit_convert(
        &mut self,
        from: OperandKind,
        to: OperandKind,
        sxtw: bool,
        make: fn(Arm64Register, Arm64Register) -> Arm64Instruction,
    ) {
        let src = self.pop_kind(from);
        let dst = self.alloc_reg(to.is_fp());
        self.buffer.emit(make(dst, src));
        if sxtw {
            self.emit_sxtw(dst);
        }
        self.push_reg(to, dst);    }

    // -- Compare / Branch ---------------------------------------------------

    /// `lcmp`: `CMP; CSET ne; CNEG lt` -- 1, 0 or -1 with no branch.
    pub(in crate::aarch64_backend) fn emit_lcmp(&mut self) {
        let rhs = self.pop_kind(OperandKind::I64);
        let lhs = self.pop_kind(OperandKind::I64);
        let dst = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::Cmp { rn: lhs, rm: rhs });
        self.buffer.emit(Arm64Instruction::Cset {
            rd: dst,
            cond: Arm64Condition::Ne,
        });
        self.buffer.emit(Arm64Instruction::Cneg {
            rd: dst,
            rn: dst,
            cond: Arm64Condition::Lt,
        });
        self.push_reg(OperandKind::I32, dst);
    }

    /// `fcmpl`/`fcmpg`/`dcmpl`/`dcmpg`: `FCMP; CSET ne; CNEG <c>`.
    ///
    /// After `FCMP`: equal is `Z=1 C=1`, less `N=1`, greater `C=1`, and
    /// unordered `C=1 V=1`. `CSET ne` gives 1 for everything but equal; the
    /// negation then decides where NaN lands:
    ///
    /// * `*cmpg` (NaN -> +1) negates on `MI` (`N=1`), which only "less" sets.
    /// * `*cmpl` (NaN -> -1) negates on `LT` (`N!=V`), which "less" and
    ///   "unordered" both satisfy.
    ///
    /// The branchy version it replaces took `B.LT` for `*cmpg`, which is TRUE
    /// on unordered, so NaN produced -1 where the JVMS requires +1.
    pub(in crate::aarch64_backend) fn emit_fcmp(&mut self, kind: OperandKind, nan_is_greater: bool) {
        let rhs = self.pop_kind(kind);
        let lhs = self.pop_kind(kind);
        let dst = self.alloc_reg(false);
        let cmp = if kind == OperandKind::F32 {
            Arm64Instruction::FcmpSingle { vn: lhs, vm: rhs }
        } else {
            Arm64Instruction::FcmpDouble { vn: lhs, vm: rhs }
        };
        self.buffer.emit(cmp);
        self.buffer.emit(Arm64Instruction::Cset {
            rd: dst,
            cond: Arm64Condition::Ne,
        });
        self.buffer.emit(Arm64Instruction::Cneg {
            rd: dst,
            rn: dst,
            cond: if nan_is_greater {
                Arm64Condition::Mi
            } else {
                Arm64Condition::Lt
            },
        });
        self.push_reg(OperandKind::I32, dst);
    }

    /// `if_icmp<cond>`: a W-form compare.
    pub fn emit_if_icmp(&mut self, cond: Arm64Condition, target_pc: usize) {
        let rhs = self.pop_kind(OperandKind::I32);
        let lhs = self.pop_kind(OperandKind::I32);
        let label = self.prepare_branch(target_pc);
        self.buffer
            .emit(Arm64Instruction::CmpW { rn: lhs, rm: rhs });
        self.buffer.emit(Arm64Instruction::BCond { cond, label });
    }

    /// `if_acmp<cond>`: references are full 64-bit words.
    pub(in crate::aarch64_backend) fn emit_if_acmp(&mut self, cond: Arm64Condition, target_pc: usize) {
        let rhs = self.pop_kind(OperandKind::Ref);
        let lhs = self.pop_kind(OperandKind::Ref);
        let label = self.prepare_branch(target_pc);
        self.buffer.emit(Arm64Instruction::Cmp { rn: lhs, rm: rhs });
        self.buffer.emit(Arm64Instruction::BCond { cond, label });
    }

    /// `if<cond>` against zero, on the W register.
    pub(in crate::aarch64_backend) fn emit_if_zero(&mut self, cond: Arm64Condition, target_pc: usize) {
        let val = self.pop_kind(OperandKind::I32);
        let label = self.prepare_branch(target_pc);
        match cond {
            Arm64Condition::Eq => self.buffer.emit(Arm64Instruction::CbzW { rt: val, label }),
            Arm64Condition::Ne => self.buffer.emit(Arm64Instruction::CbnzW { rt: val, label }),
            _ => {
                self.buffer
                    .emit(Arm64Instruction::CmpImmW { rn: val, imm: 0 });
                self.buffer.emit(Arm64Instruction::BCond { cond, label });
            }
        }
    }

    /// `ifnull` / `ifnonnull`.
    pub(in crate::aarch64_backend) fn emit_if_null(&mut self, nonnull: bool, target_pc: usize) {
        let val = self.pop_kind(OperandKind::Ref);
        let label = self.prepare_branch(target_pc);
        if nonnull {
            self.buffer.emit(Arm64Instruction::Cbnz { rt: val, label });
        } else {
            self.buffer.emit(Arm64Instruction::Cbz { rt: val, label });
        }
    }

    // -- Load / Store locals ------------------------------------------------

    /// Size the frame from a spill-area partition and record both.
    ///
    /// The only place `self.frame` and `self.spill_area` are set, so the total
    /// the frame is sized from is derived from the same partition the
    /// accessors index against and cannot drift from it. A test that builds a
    /// frame goes through here too: a frame without its partition is a frame
    /// production never builds, and letting a test construct one is how a
    /// fixture ends up proving something about a shape that cannot occur.
    pub(in crate::aarch64_backend) fn install_frame(&mut self, num_locals: usize, area: Arm64SpillArea, saved: &[Arm64Register]) {
        self.spill_area = area;
        self.frame = Some(Arm64FrameLayout::compute(num_locals, area.total(), saved));
    }

    /// FP-relative offset of spill-area word `word`, or `None` when there is no
    /// frame yet or the word index is `None`.
    ///
    /// The ONE place a word index becomes a byte offset. Every caller gets its
    /// index from an [`Arm64SpillArea`] accessor, which has already bounded it
    /// against its own region, so this function's only job is the arithmetic.
    pub(in crate::aarch64_backend) fn spill_word_offset(&self, word: Option<usize>) -> Option<i32> {
        let frame = self.frame.as_ref()?;
        let scaled = i32::try_from(word?).ok()?.checked_mul(8)?;
        frame.spill_offset.checked_add(scaled)
    }

    /// FP-relative offset of frame-homed local `index`'s word.
    pub(in crate::aarch64_backend) fn local_slot_offset(&self, index: usize) -> Option<i32> {
        let word = self.spill_area.local_word(self.spill_index_for(index));
        self.spill_word_offset(word)
    }

    /// Push a copy of local `index`, read as a `kind`.
    pub(in crate::aarch64_backend) fn emit_load_local(&mut self, index: usize, kind: OperandKind) {
        let Some(home) = self.local_regs.get(index).copied() else {
            self.failed = true;
            return;
        };
        let dst = self.alloc_reg(kind.is_fp());
        match home {
            Some(reg) => {
                let inst = match kind {
                    OperandKind::F32 => Arm64Instruction::FmovToFpSingle { vd: dst, rn: reg },
                    OperandKind::F64 => Arm64Instruction::FmovToFp { vd: dst, rn: reg },
                    _ => Arm64Instruction::Mov { rd: dst, rm: reg },
                };
                self.buffer.emit(inst);
            }
            None => {
                let Some(offset) = self.local_slot_offset(index) else {
                    self.failed = true;
                    return;
                };
                self.emit_load_kind(dst, kind, offset);            }        }
        self.push_reg(kind, dst);
    }

    /// Pop a `kind` and store it to local `index`.
    pub(in crate::aarch64_backend) fn emit_store_local(&mut self, index: usize, kind: OperandKind) {
        let src = self.pop_kind(kind);
        let Some(home) = self.local_regs.get(index).copied() else {
            self.failed = true;
            return;
        };
        match home {
            Some(reg) => {
                let inst = match kind {
                    OperandKind::F32 => Arm64Instruction::FmovFromFpSingle { rd: reg, vn: src },
                    OperandKind::F64 => Arm64Instruction::FmovFromFp { rd: reg, vn: src },
                    _ => Arm64Instruction::Mov { rd: reg, rm: src },
                };
                self.buffer.emit(inst);
            }
            None => {
                let Some(offset) = self.local_slot_offset(index) else {
                    self.failed = true;
                    return;
                };
                self.emit_store_kind(src, kind, offset);            }        }    }

    /// How many spill slots the FRAME-HOMED LOCALS occupy -- equivalently, the
    /// first spill index the operand area uses. Both the operand area and the
    /// safepoint homes take their base from here, or they overlap the locals
    /// (see `operand_spill_slots_do_not_alias_frame_homed_locals`).
    pub(in crate::aarch64_backend) fn local_spill_count(&self) -> usize {
        self.spill_index_for(self.local_regs.len())
    }

    /// The spill index of a local that has no register: how many locals before
    /// it also have none.
    pub(in crate::aarch64_backend) fn spill_index_for(&self, local_index: usize) -> usize {
        let mut spill_idx = 0;
        for i in 0..local_index {
            let has_gpr = self.local_regs.get(i).map_or(false, |r| r.is_some());
            let has_fp = self.float_local_regs.get(i).map_or(false, |r| r.is_some());
            if !has_gpr && !has_fp {
                spill_idx += 1;
            }
        }
        spill_idx
    }

    // -- Constants ----------------------------------------------------------

    /// An `int` constant, sign-extended -- already the canonical form.
    pub fn emit_iconst(&mut self, value: i32) {
        let dst = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: dst,
            imm: i64::from(value),
        });
        self.push_reg(OperandKind::I32, dst);
    }

    pub fn emit_lconst(&mut self, value: i64) {
        let dst = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: dst,
            imm: value,
        });
        self.push_reg(OperandKind::I64, dst);
    }

    /// A `float` constant, bit-exact: its IEEE-754 single pattern into a GPR,
    /// then `FMOV Sd, Wn` (a bit move, not a conversion). It used to load the
    /// DOUBLE pattern, so every `float` was a `double` in disguise.
    pub fn emit_fconst(&mut self, value: f32) {
        let tmp = self.alloc_reg(false);
        let dst = self.alloc_reg(true);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: tmp,
            imm: i64::from(value.to_bits()),
        });
        self.buffer
            .emit(Arm64Instruction::FmovToFpSingle { vd: dst, rn: tmp });
        self.push_reg(OperandKind::F32, dst);
    }

    /// A `double` constant, bit-exact via `FMOV Dd, Xn`.
    pub fn emit_dconst(&mut self, value: f64) {
        let tmp = self.alloc_reg(false);
        let dst = self.alloc_reg(true);
        // Cast: reinterpret the f64 as its raw 64-bit pattern.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: tmp,
            imm: value.to_bits() as i64,
        });
        self.buffer
            .emit(Arm64Instruction::FmovToFp { vd: dst, rn: tmp });
        self.push_reg(OperandKind::F64, dst);
    }

    // -- Static fields (round 9 wave 10) ------------------------------------

    /// `getstatic` of a resolved PRIMITIVE static: the x64 inline
    /// `getstatic`'s shape (`x64/objects.rs` `try_emit_inline_getstatic`)
    /// with the aarch64 memory model applied.
    ///
    /// ```text
    ///   MOV   Xa, #base_cell         ; &statics_index[class].base
    ///   LDR   Xa, [Xa]               ; the class's statics block
    ///   ADD   Xa, Xa, #(field_index*SLOT_SIZE + payload)
    ///   LDR   Wa|Xa, [Xa]            ; LDAR for a volatile field
    ///   SXTW  Xa, Wa                 ; int category: this backend's int form
    ///   FMOV  Sv|Dv, Wa|Xa           ; float / double: into an FP register
    /// ```
    ///
    /// The cell is the 16-byte `Value` layout: the int category (`Z B C S I`)
    /// and `F` read their 32-bit payload at `FIELD_CELL_PAYLOAD32_OFFSET`, `J`
    /// and `D` the 64-bit one at `FIELD_CELL_PAYLOAD64_OFFSET` -- the same
    /// loads, widths and result conventions as x64 (`MOVSXD` for the int
    /// category is `LDR W` + `SXTW` here). A 64-bit `LDAR` of the 8-aligned
    /// payload is single-copy atomic, which JLS §17.7 requires of a `volatile
    /// long`/`double`.
    ///
    /// JMM: a `volatile` read is `LDAR` and nothing more. ARMv8 orders a
    /// prior `STLR` before a later `LDAR` (RCsc), so the StoreLoad edge is the
    /// writer's to pay, exactly as x64 puts its `MFENCE` after the volatile
    /// STORE and none after the load. A plain read needs no ordering.
    ///
    /// # Reference statics (h23, 2026-09-22)
    ///
    /// `L` and `[` read the SAME 64-bit payload word as `J`/`D` and push it as
    /// an `OperandKind::Ref`, which is exactly what x64's
    /// `try_emit_inline_getstatic` does (`b'J' | b'D' | b'L' | b'['` share one
    /// arm there, followed by `mark_top_as_oop`). The oop story the earlier
    /// refusal was waiting for is the operand model's, and it already exists:
    /// a `Ref` entry carries `oop: true` (`Operand::in_reg`), the shuffles
    /// carry the mark, and a safepoint spills the entry to its depth slot,
    /// which is what `local_oop_mask_at_current_pc` publishes. No load barrier
    /// is owed: every collector in this VM reads a static's payload word raw
    /// from compiled code, which is the contract the inline `getfield` arms
    /// and x64's own reference `getstatic` already run under. `Object(None)`
    /// leaves the payload word zero, i.e. JVM null, so no decode is needed.
    ///
    /// A `volatile` reference static gets the same `LDAR` as any other
    /// volatile read here, which is stronger than the plain `MOV` x64 emits
    /// under TSO and correct for the same reason.
    ///
    /// Returns `false`, having emitted nothing, for an unknown type tag, a
    /// zero `base_cell`, or an offset that does not fit; the caller refuses
    /// the method.
    pub(in crate::aarch64_backend) fn emit_getstatic(&mut self, field: Arm64StaticField) -> bool {
        use cratonvm_types::{FIELD_CELL_PAYLOAD32_OFFSET, FIELD_CELL_PAYLOAD64_OFFSET, SLOT_SIZE};
        let (kind, width, payload) = match field.type_tag {
            b'L' | b'[' => (
                OperandKind::Ref,
                Arm64MemWidth::X64,
                FIELD_CELL_PAYLOAD64_OFFSET,
            ),
            b'Z' | b'B' | b'C' | b'S' | b'I' => (
                OperandKind::I32,
                Arm64MemWidth::W32,
                FIELD_CELL_PAYLOAD32_OFFSET,
            ),
            b'F' => (
                OperandKind::F32,
                Arm64MemWidth::W32,
                FIELD_CELL_PAYLOAD32_OFFSET,
            ),
            b'J' => (
                OperandKind::I64,
                Arm64MemWidth::X64,
                FIELD_CELL_PAYLOAD64_OFFSET,
            ),
            b'D' => (
                OperandKind::F64,
                Arm64MemWidth::X64,
                FIELD_CELL_PAYLOAD64_OFFSET,
            ),
            _ => return false,
        };
        if field.base_cell == 0 {
            return false;
        }
        let Some(cell_off) = field
            .field_index
            .checked_mul(SLOT_SIZE)
            .and_then(|off| off.checked_add(payload))
            .and_then(|off| i32::try_from(off).ok())
        else {
            return false;
        };
        let addr = self.alloc_reg(false);
        // Cast: the pointer cell's address as the 64-bit pattern `MovImm`
        // materializes.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: addr,
            imm: field.base_cell as i64,
        });
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: addr,
            rn: addr,
            offset: 0,
        });
        // `cell_off` is never 0 (the payload offsets are 4 and 8), but the
        // pseudo-op costs nothing to skip if a layout ever makes it so.
        if cell_off != 0 {
            self.buffer.emit(Arm64Instruction::AddImm {
                rd: addr,
                rn: addr,
                imm: cell_off,
            });
        }
        self.buffer.emit(Arm64Instruction::MemLoad {
            rt: addr,
            rn: addr,
            width,
            acquire: field.is_volatile,
        });
        match kind {
            OperandKind::I32 => {
                self.emit_sxtw(addr);
                self.push_reg(OperandKind::I32, addr);
            }
            OperandKind::F32 => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFpSingle { vd: v, rn: addr });
                self.push_reg(OperandKind::F32, v);
            }
            OperandKind::F64 => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFp { vd: v, rn: addr });
                self.push_reg(OperandKind::F64, v);
            }
            OperandKind::Ref => {
                self.push_reg(OperandKind::Ref, addr);
                // Belt and braces with `Operand::in_reg`'s own `kind == Ref`,
                // and the same line the reference-returning `invoke` arm
                // writes: the mark is what a safepoint's oop map is built from.
                self.mark_top_operand_as_oop();
            }
            // `I64` (the only other kind the table above produces).
            _ => self.push_reg(kind, addr),
        }
        self.static_init_classes.push(field.class_id);
        true
    }

    // -- Mid-method helper calls --------------------------------------------

    /// Call a runtime helper from the MIDDLE of a method, preserving the
    /// operand stack across it (round 9 wave 15).
    ///
    /// ```text
    ///     <store every register-located operand to its depth slot>
    ///     MOV       X0..X7, <arguments>      ; or MOVZ/MOVK for a constant
    ///     MOVZ/MOVK X16, #helper
    ///     BLR       X16
    ///     MOV       Xinto, X0                ; when the helper returns a value
    ///     <reload the operands into the registers the model says they are in>
    /// ```
    ///
    /// # Why this is not just the safepoint poll's sequence
    ///
    /// Until this landed, every call this backend emitted was one of two
    /// shapes that dodge the hard part. `emit_frame_record` and the entry poll
    /// run with an EMPTY operand stack -- `emit_frame_record` refuses the
    /// method outright when it is not, which is the admission that it cannot
    /// preserve one. The throw stubs (`emit_arith_throw_stub`,
    /// `emit_npe_throw_stubs`, `emit_aioobe_stubs`) marshal, call and branch
    /// to the epilogue: nothing has to survive, because the frame is leaving.
    /// The loop-header poll is the one that does preserve a stack, and it gets
    /// the join for free by being unconditional, argument-less and
    /// result-less.
    ///
    /// # The CALLER allocates the result register, and allocates it FIRST
    ///
    /// `into` is a register the caller already took from [`Self::alloc_reg`],
    /// and this function refuses one that still holds an operand. Both halves
    /// matter, for different reasons.
    ///
    /// Allocating it FIRST is what keeps it out of the reload set: `alloc_reg`
    /// spills whatever operand occupies the register it hands out, so an
    /// allocation made before the spill loop has already moved that entry to
    /// its slot. Allocating after can hand back a register that is about to be
    /// reloaded, and the reload then overwrites the helper's result with the
    /// operand -- silently, because both are legitimate values of the same
    /// width. The refusal here is what makes that a checked rule rather than a
    /// comment.
    ///
    /// Allocating it in the CALLER is what makes a call inside a branch
    /// possible. `getfield`'s sentinel disambiguation calls a second helper on
    /// the rare `X0 == i64::MIN` path; if this function allocated, that
    /// allocation would happen on one path only and could spill an operand on
    /// one path only, so the two paths would reach the join with different
    /// operand models. With the caller allocating before the branch, both
    /// paths are identical by construction.
    ///
    /// # What this does NOT do, and what a caller therefore owes
    ///
    /// * **No oop map.** The reference operands are in their depth slots and
    ///   the reference LOCALS are in callee-saved X19-X28, where a relocating
    ///   collector can neither find nor rewrite them. That is sound only for a
    ///   helper that cannot safepoint, which every caller must establish for
    ///   ITS helper -- `jit_putfield_*` and `jit_getfield` allocate nothing and
    ///   run no Java, and x64 records no map at those call sites either. A
    ///   helper that CAN safepoint (`tlab_post_init`, the `invoke*`
    ///   dispatchers) needs the poll's treatment instead: home the
    ///   register-homed reference locals, record a map at the return address,
    ///   and reload them afterwards.
    /// * **No sentinel check.** A helper that signals an exception by
    ///   returning `i64::MIN` needs its caller to test for it.
    ///
    /// Answers `false`, having set `self.failed`, when it refuses.
    pub(in crate::aarch64_backend) fn emit_helper_call(
        &mut self,
        helper: usize,
        args: &[Arm64HelperArg],
        into: Option<Arm64Register>,
    ) -> bool {
        if helper == 0 {
            // An unwired helper would make this a call through a null pointer.
            self.failed = true;
            return false;
        }
        if args.len() > Arm64EntryConvention::INT_ARG_REGS.len() {
            // A ninth argument goes on the stack, which this sequence does not
            // build. No helper called from here takes one.
            self.failed = true;
            return false;
        }
        for arg in args {
            if let Arm64HelperArg::Reg(reg) = *arg {
                if !SCRATCH_REGS.contains(&reg) {
                    // See `Arm64HelperArg`: a source outside the scratch pool
                    // could alias an argument register.
                    self.failed = true;
                    return false;
                }
            }
        }
        if let Some(dst) = into {
            if !SCRATCH_REGS.contains(&dst) || self.reg_is_live(dst) {
                // See the doc comment: a destination that still holds an
                // operand is one the reload below would overwrite.
                self.failed = true;
                return false;
            }
        }

        // The operand stack, GPR and FP alike: X9-X15 and V0-V7 are all
        // caller-saved, so the call destroys every one of them.
        let mut stored: Vec<(Arm64Register, OperandKind, i32)> = Vec::new();
        for depth in 0..self.operand_stack.len() {
            let operand = self.operand_stack[depth];
            let OperandLoc::Reg(reg) = operand.loc else {
                continue;
            };
            let Some(offset) = self.spill_offset_for_depth(depth) else {
                // No slot for this depth: a live value would sit in a
                // caller-saved register across a CALL. Refuse the method.
                self.failed = true;
                return false;
            };
            self.emit_store_kind(reg, operand.kind, offset);
            stored.push((reg, operand.kind, offset));
        }

        // The arguments. X0-X7 are disjoint from the scratch pool the sources
        // come out of (checked above), so no order of these moves can destroy
        // a source a later one still reads.
        for (i, arg) in args.iter().enumerate() {
            let dst = Arm64EntryConvention::INT_ARG_REGS[i];
            match *arg {
                Arm64HelperArg::Reg(src) => {
                    self.buffer.emit(Arm64Instruction::Mov { rd: dst, rm: src });
                }
                Arm64HelperArg::Imm(imm) => {
                    self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm });
                }
            }
        }

        // Cast: a helper address is a real mapped pointer, always < i64::MAX.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: helper as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });

        // Out of X0 BEFORE the reloads. They target scratch registers and X0
        // is not one, so the order is not load-bearing today; it is written
        // this way so that it cannot become load-bearing unnoticed.
        if let Some(dst) = into {
            self.buffer.emit(Arm64Instruction::Mov {
                rd: dst,
                rm: Arm64Register::X0,
            });
        }

        for (reg, kind, offset) in stored {
            self.emit_load_kind(reg, kind, offset);
        }
        true
    }

    /// `putstatic` (`0xb3`) of a PRIMITIVE static, round 9 wave 17.
    ///
    /// ```text
    ///   FMOV  Xval, Dval                  ; F/D only: the helper takes bits
    ///   <narrow the value to the field's declared type>
    ///   LDR   Xctx, [FP, #context]
    ///   DMB   ISH                         ; volatile only
    ///   <emit_helper_call(jit_putstatic_*, [ctx, class_id, field_index, val]) -> Xr>
    ///   MOVZ/MOVK Xs, #i64::MIN
    ///   CMP   Xr, Xs
    ///   B.NE  join                        ; 0 is the helper's success value
    ///   MOV   X0, #i64::MIN               ; `<clinit>` failed -- leave
    ///   B     epilogue
    /// join:
    ///   DMB   ISH                         ; volatile only
    /// ```
    ///
    /// # Why this is a CALL when `getstatic` two arms up is two loads
    ///
    /// A raw store into the statics block races `set_static_shared`'s
    /// `grow_to`, which copies the block under the write lock and republishes
    /// it: a compiled store into the OLD block between the copy and the
    /// republish is simply lost. The helper takes that lock, and taking a lock
    /// is a call. The read has no such hazard, which is why `getstatic` can be
    /// the cell read plus one load and this cannot be its mirror image.
    ///
    /// # Why no oop map, when this helper CAN run `<clinit>`
    ///
    /// `ensure_class_initialized_shared` runs Java, and running Java can
    /// safepoint and can move objects -- which by [`Self::emit_helper_call`]'s
    /// stated obligation would mean homing the reference locals, recording a
    /// map at the return address and reloading them. It does not, because
    /// **the class is already initialized at compile time**: the caller
    /// resolves a `base_cell` through `DirectHelperTable::resolve_static_base`,
    /// which answers only for an initialized class, and this arm refuses a
    /// site without one. Initialization is monotonic, so the class cannot
    /// become uninitialized before the body runs -- and the declaring class is
    /// additionally recorded in `CompiledMethod::static_init_classes`, which
    /// the interpreter's compiled-entry path ensure-initializes before first
    /// execution. The `<clinit>` branch inside the helper is therefore
    /// unreachable from here, and the sentinel check below is the belt to that
    /// pair of braces.
    ///
    /// # The sentinel is an EXCEPTION, not a re-run
    ///
    /// `jit_putstatic_*` returns `0` on success and `i64::MIN` only after
    /// `set_jit_pending_exception` -- so leaving through the epilogue with it
    /// raises that exception rather than re-running the method from its entry,
    /// which is what makes this safe without a deopt point. The comparison is
    /// against `i64::MIN` specifically, not `!= 0`: a spurious bail would
    /// leave with the sentinel and NO pending exception, which the interpreter
    /// would read as a deopt and re-run -- double-executing this very store.
    ///
    /// # `DMB ISH` on BOTH sides of a volatile store
    ///
    /// The same asymmetry as `putfield`, and for the same reason: x64 emits
    /// only the trailing `MFENCE` because TSO already makes its plain `MOV` a
    /// release, while here the store happens inside the helper and a `BLR`
    /// orders nothing.
    ///
    /// A REFERENCE static takes the same shape through
    /// `jit_putstatic_object`, which adds the SATB pre-barrier and the card
    /// mark (round 9 wave 19).
    ///
    /// Returns `false`, and the caller refuses the method, for an unresolved
    /// site, an unwired helper, a non-empty exception table, or no context
    /// word.
    pub(in crate::aarch64_backend) fn emit_putstatic(&mut self, field: Arm64StaticField) -> bool {
        // A REFERENCE static takes the same shape with a different helper:
        // `jit_putstatic_object` runs the SATB pre-barrier and the card mark
        // the primitive stores do not owe, and takes the value as a pointer
        // rather than as a narrowed word (round 9 wave 19).
        let is_ref = matches!(field.type_tag, b'L' | b'[');
        let kind = if is_ref {
            OperandKind::Ref
        } else {
            let Some(kind) = primitive_field_operand_kind(field.type_tag) else {
                return false;
            };
            kind        };
        let helper = match field.type_tag {
            b'L' | b'[' => self.helpers.putstatic_object,
            b'J' => self.helpers.putstatic_long,
            b'F' => self.helpers.putstatic_float,
            b'D' => self.helpers.putstatic_double,
            _ => self.helpers.putstatic_int,
        };
        // `base_cell` is not READ here -- the helper does the store. It is the
        // caller's proof that the declaring class is already initialized; see
        // the doc comment.
        if helper == 0 || field.base_cell == 0 {
            return false;
        }
        // The sentinel path below leaves through the epilogue for the drain to
        // raise. Wave 17 shipped without a guard here at all: the `<clinit>`
        // branch it reasons is unreachable is not the only way the helper
        // returns the sentinel, because `contain(.., OnPanic::Throw,
        // i64::MIN, ..)` returns it for a PANIC as well. `can_route_exception`
        // is what makes the drain's throw exact regardless -- see its doc.
        if !self.can_route_exception() {
            return false;
        }
        let Some(ctx_off) = self.context_slot_offset() else {
            return false;
        };
        let Ok(index) = i64::try_from(field.field_index) else {
            return false;
        };

        let val = self.pop_kind(kind);
        if self.failed {
            return false;
        }
        // The helpers take `(vm_ptr, class_id, field_index, val: i64)`, so a
        // float or double travels in a GPR as its bit pattern. A reference is
        // already one.
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
        // EVERY allocation before the branch. See `emit_helper_call`.
        let ctx = self.alloc_reg(false);
        let ret = self.alloc_reg(false);
        let sentinel = self.alloc_reg(false);
        if self.failed {
            return false;
        }
        // JVMS §6.5: a `boolean` static stores `value & 1` and a
        // `byte`/`char`/`short` static only its own width. `jit_putstatic_int`
        // stores its operand verbatim and has no descriptor to narrow by, so
        // it happens here -- exactly as x64's `0xb3` arm does it. A reference
        // has no narrowing: `emit_narrow_to_field_tag` answers nothing for
        // `L`/`[`, and this says so rather than relying on it.
        if !is_ref {
            self.emit_narrow_to_field_tag(val, field.type_tag);
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
            helper,
            &[
                Arm64HelperArg::Reg(ctx),
                Arm64HelperArg::Imm(i64::from(field.class_id)),
                Arm64HelperArg::Imm(index),
                Arm64HelperArg::Reg(val),
            ],
            Some(ret),
        ) {
            return false;
        }
        let join = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: sentinel,
            imm: i64::MIN,
        });
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: ret,
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
        if field.is_volatile {
            self.buffer.emit(Arm64Instruction::DmbIsh);
        }
        // The ensure-initialized obligation, as x64 records it for every
        // static site including every `putstatic`.
        self.static_init_classes.push(field.class_id);
        true
    }

    // -- Arrays -------------------------------------------------------------

    /// `arraylength` (`0xbe`), round 9 wave 12. The second ordered lowering,
    /// and the first one with a RECEIVER.
    ///
    /// ```text
    ///   CBZ   Xa, npe_ARRAY_LENGTH        ; JVMS: a null array throws NPE
    ///   ADD   Xd, Xa, #ARRAY_LENGTH_OFFSET
    ///   LDR   Wd, [Xd]                    ; the header's `shape` word
    ///   SXTW  Xd, Wd                      ; this backend's canonical int form
    /// ```
    ///
    /// x64 emits the same two steps (`x64/arrays.rs::emit_arraylength_regs`,
    /// `MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]` behind a null check). The
    /// 32-bit load ZERO-extends and the `SXTW` then sign-extends, which agree
    /// because an array length is a non-negative `int`: the allocator refuses
    /// a negative one (`NegativeArraySizeException`), so bit 31 is clear.
    ///
    /// # Why a PLAIN `LDR` is the ordered lowering on a weakly-ordered machine
    ///
    /// This is the part that is not obvious from x64. An array's length is
    /// written once, by the allocator, before the reference is published, and
    /// never again -- it is a `final` field in all but name. The hazard a weak
    /// memory model adds is reading the length through a freshly published
    /// reference and seeing the pre-initialization value. AArch64 cannot do
    /// that here: the length load's ADDRESS is computed FROM the reference,
    /// so it is address-dependent on the load that produced the reference, and
    /// ARMv8 never reorders a load before the load its address depends on
    /// (§B2.3.2, "dependency-ordered before"). That is exactly the guarantee
    /// the JMM's freeze action needs, and it is why no `LDAR` and no
    /// `DMB ISHLD` is owed -- not because x64 emits none.
    ///
    /// Returns `false`, having emitted nothing, when there is no exact NPE
    /// path ([`Self::can_throw_npe`]); the caller refuses the method.
    pub(in crate::aarch64_backend) fn emit_arraylength(&mut self) -> bool {
        if !self.can_throw_npe() {
            return false;
        }
        let arr = self.pop_kind(OperandKind::Ref);
        self.emit_null_check(arr, npe_action::ARRAY_LENGTH);
        // `arr` is HELD, so `dst` is a different register and the `ADD` cannot
        // clobber the receiver the stub above still names.
        let dst = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: dst,
            rn: arr,
            // Cast: a small header constant, const-asserted <= 127 in
            // `cratonvm_types`.
            imm: cratonvm_types::ARRAY_LENGTH_OFFSET as i32,
        });
        self.buffer.emit(Arm64Instruction::MemLoad {
            rt: dst,
            rn: dst,
            width: Arm64MemWidth::W32,
            acquire: false,
        });
        self.emit_sxtw(dst);
        self.push_reg(OperandKind::I32, dst);
        true
    }

    // -- Arrays (round 9 wave 13) -------------------------------------------

    /// Can this compilation raise `ArrayIndexOutOfBoundsException` from
    /// compiled code? Unlike [`Self::can_throw_arithmetic`] and
    /// [`Self::can_throw_npe`], only the helper needs to be wired:
    /// `jit_throw_aioobe` takes the throwing bci as its own fourth argument
    /// (`emit_aioobe_stubs`) and stamps `JIT_SIGNALS::athrow_bci` with it
    /// directly (`vm/src/jit/helpers.rs`), the same cell
    /// [`Self::emit_stamp_throw_bci`] writes for the lowerings that need it --
    /// so the drain always has an exact throw site here, table empty or not.
    pub(in crate::aarch64_backend) fn can_throw_aioobe(&self) -> bool {
        self.helpers.throw_aioobe != 0
    }

    /// Both throw paths an array element access needs: a null receiver is an
    /// NPE and an out-of-range index an AIOOBE, and a lowering that can raise
    /// only one of them is not a lowering.
    pub(in crate::aarch64_backend) fn can_access_array(&self) -> bool {
        self.can_throw_npe() && self.can_throw_aioobe()
    }

    /// Null-check `arr`, bounds-check `idx` against its length, and compute
    /// the element address `arr + ARRAY_DATA_OFFSET + (idx << shift)`.
    ///
    /// ```text
    ///   CBZ   Xarr, npe_<action>
    ///   ADD   Xlen, Xarr, #ARRAY_LENGTH_OFFSET
    ///   LDR   Wlen, [Xlen]
    ///   CMP   Widx, Wlen
    ///   B.HS  aioobe_<site>          ; UNSIGNED: catches idx < 0 too
    ///   ADD   Xaddr, Xarr, Xidx, LSL #shift
    ///   ADD   Xaddr, Xaddr, #ARRAY_DATA_OFFSET
    /// ```
    ///
    /// The unsigned compare is x64's trick (`CMP ECX, [len]` + `JAE`) and for
    /// the same reason: a negative index reads as a huge unsigned value, so
    /// one branch covers both halves of the JVMS check. `idx` arrives in this
    /// backend's canonical int form (`SXTW`), so its W view is the `int` and
    /// its X view is the correct 64-bit scale factor once the check has
    /// proved it non-negative.
    ///
    /// Neither the length load nor the element access is ordered, and that is
    /// the lowering, not an omission: both addresses are derived from the
    /// array reference, so ARMv8 orders them after the load that produced it
    /// (address dependency), and a plain array element is ordinary memory the
    /// JMM owes nothing about -- exactly what x64 emits plain `MOV`s for.
    ///
    /// Returns the address register, or `None` (having set `failed`) when the
    /// registers it would name are not the scratch ones the stub assumes.
    pub(in crate::aarch64_backend) fn emit_array_guards_and_address(
        &mut self,
        arr: Arm64Register,
        idx: Arm64Register,
        action: u8,
        shift: u8,
    ) -> Option<Arm64Register> {
        self.emit_null_check(arr, action);
        // `arr` and `idx` are HELD, so `len` is neither of them.
        let len = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: len,
            rn: arr,
            // Cast: a small header constant, const-asserted <= 127.
            imm: cratonvm_types::ARRAY_LENGTH_OFFSET as i32,
        });
        self.buffer.emit(Arm64Instruction::MemLoad {
            rt: len,
            rn: len,
            width: Arm64MemWidth::W32,
            acquire: false,
        });
        self.buffer
            .emit(Arm64Instruction::CmpW { rn: idx, rm: len });
        // The stub reads these three registers, so they must be ones nothing
        // between the branch and the stub can be holding: the argument
        // registers the stub writes (X0-X3, X16) are disjoint from the scratch
        // pool by construction, and this is the assertion of that.
        if !SCRATCH_REGS.contains(&arr)
            || !SCRATCH_REGS.contains(&idx)
            || !SCRATCH_REGS.contains(&len)
        {
            self.failed = true;
            return None;
        }
        let label = self.buffer.new_label();
        self.aioobe_stubs.push(Arm64AioobeStub {
            label,
            index: idx,
            length: len,
            array: arr,
            bci: self.cur_bytecode_pc,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Cs,
            label,
        });
        let addr = self.alloc_reg(false);
        if shift == 0 {
            self.buffer.emit(Arm64Instruction::Add {
                rd: addr,
                rn: arr,
                rm: idx,
            });
        } else {
            self.buffer.emit(Arm64Instruction::AddLsl {
                rd: addr,
                rn: arr,
                rm: idx,
                shift,
            });
        }
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: addr,
            rn: addr,
            // Cast: a small header constant, const-asserted <= 127.
            imm: cratonvm_types::ARRAY_DATA_OFFSET as i32,
        });
        Some(addr)
    }

    /// The guards WITHOUT the address: the null check and the unsigned bounds
    /// compare, and nothing else (round 9 wave 19).
    ///
    /// `aaload` and `aastore` want exactly this. They do not address the
    /// element themselves -- only the helper knows its encoding -- and the
    /// address register [`Self::emit_array_guards_and_address`] allocates is
    /// not free: every register it hands out is HELD for the rest of the
    /// bytecode, because the AIOOBE stub reads the guard's registers by name
    /// when it is emitted at the end of the pass. One unnecessary hold is one
    /// fewer register for a bytecode that already needs the array, the index,
    /// the value, the context and a result across two calls.
    pub(in crate::aarch64_backend) fn emit_array_guards_only(
        &mut self,
        arr: Arm64Register,
        idx: Arm64Register,
        action: u8,
    ) -> bool {
        self.emit_array_guards_and_address(arr, idx, action, 0)
            .is_some()
    }

    /// Emit every `ArrayIndexOutOfBoundsException` stub this pass branched to,
    /// after the epilogue's `RET`.
    ///
    /// ```text
    ///   aioobe_<site>:
    ///     MOV       X0, Xidx        ; index   (i64, sign-extended int)
    ///     MOV       X1, Xlen        ; length  (i64, zero-extended u32)
    ///     MOV       X2, Xarr        ; the array, for the helper's diagnostics
    ///     MOVZ/MOVK X3, #bci
    ///     MOVZ/MOVK X16, #jit_throw_aioobe
    ///     BLR       X16             ; returns the i64::MIN deopt sentinel
    ///     B         epilogue
    /// ```
    ///
    /// ONE stub per SITE, unlike the NPE stubs' one per action: the helper
    /// takes the index, the length, the array and the bci, so two sites agree
    /// on none of its four arguments. The three register arguments are read
    /// from where the guard left them, which is sound precisely because the
    /// only way into the stub is that guard's `B.HS`.
    ///
    /// Not a safepoint: the helper allocates nothing and runs no Java, and
    /// this frame is leaving through the epilogue -- the same argument as
    /// [`Self::emit_arith_throw_stub`].
    pub(in crate::aarch64_backend) fn emit_aioobe_stubs(&mut self) {
        let stubs = std::mem::take(&mut self.aioobe_stubs);
        for stub in stubs {
            self.buffer.bind_label(stub.label);
            for (rd, rm) in [
                (Arm64Register::X0, stub.index),
                (Arm64Register::X1, stub.length),
                (Arm64Register::X2, stub.array),
            ] {
                self.buffer.emit(Arm64Instruction::Mov { rd, rm });
            }
            let Ok(bci) = i64::try_from(stub.bci) else {
                self.failed = true;
                return;
            };
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X3,
                imm: bci,
            });
            // Cast: a helper address is a real mapped pointer, always < i64::MAX.
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X16,
                imm: self.helpers.throw_aioobe as i64,
            });
            self.buffer.emit(Arm64Instruction::Blr {
                rn: Arm64Register::X16,
            });
            self.buffer.emit(Arm64Instruction::B {
                label: self.epilogue_label,
            });
        }
    }

    /// JVMS §6.5 `bastore`: into a `boolean[]` the `int` is narrowed by
    /// `value & 1`; into a `byte[]` it is truncated to its low byte. One
    /// opcode serves both and the verifier accepts either, so the element type
    /// is a RUNTIME question -- the aarch64 twin of x64's
    /// `emit_bastore_boolean_mask_regs`, reading the same header byte:
    ///
    /// ```text
    ///   LDRB  Wt, [Xarr, #KIND_TAGS_BYTE_OFFSET]
    ///   CMP   Wt, #tag("[Z")
    ///   B.NE  skip
    ///   AND   Xval, Xval, #1
    ///   skip:
    /// ```
    ///
    /// The `AND` is the 64-bit form on purpose: `val` is a canonical
    /// sign-extended `int`, and `& 1` leaves 0 or 1, which is canonical too.
    /// `STRB` then stores the low byte either way, so the `byte[]` path is
    /// byte-identical to no mask at all.
    pub(in crate::aarch64_backend) fn emit_bastore_boolean_mask(&mut self, arr: Arm64Register, val: Arm64Register) {
        let Some(tag) = cratonvm_types::primitive_array_kind_tags_byte("[Z") else {
            // Cannot answer `None` for `"[Z"`; if it ever does, emit nothing
            // and keep the low-byte truncation rather than mask a `byte[]`.
            return;
        };
        let probe = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::Ldrb {
            rt: probe,
            rn: arr,
            // Cast: a small header constant, well inside the byte load's imm12.
            offset: cratonvm_types::KIND_TAGS_BYTE_OFFSET as i32,
        });
        self.buffer.emit(Arm64Instruction::CmpImmW {
            rn: probe,
            imm: i32::from(tag),
        });
        let skip = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Ne,
            label: skip,
        });
        self.buffer.emit(Arm64Instruction::AndImm {
            rd: val,
            rn: val,
            imm: 1,
        });
        self.buffer.bind_label(skip);
    }

    /// `iaload`/`laload`/`faload`/`daload`/`baload`/`caload`/`saload`
    /// (round 9 wave 13). `aaload` is not here: see
    /// [`array_element_load_shape`].
    ///
    /// Guards, then one load of the element's own width, then the narrowing
    /// JVMS specifies for the sub-word kinds. `MemLoad` ZERO-extends every
    /// width, so `baload` and `saload` owe an explicit `SXTB`/`SXTH`; `caload`
    /// owes nothing, a `char` being unsigned.
    pub(in crate::aarch64_backend) fn emit_array_load(&mut self, opcode: u8) -> bool {
        let Some(shape) = array_element_load_shape(opcode) else {
            return false;
        };
        if !self.can_access_array() {
            return false;
        }
        let idx = self.pop_kind(OperandKind::I32);
        let arr = self.pop_kind(OperandKind::Ref);
        let Some(addr) = self.emit_array_guards_and_address(arr, idx, shape.action, shape.shift)
        else {
            return false;
        };
        self.buffer.emit(Arm64Instruction::MemLoad {
            rt: addr,
            rn: addr,
            width: shape.width,
            acquire: false,
        });
        match shape.kind {
            OperandKind::I32 => {
                match shape.width {
                    Arm64MemWidth::B8 => self
                        .buffer
                        .emit(Arm64Instruction::Sxtb { rd: addr, rn: addr }),
                    Arm64MemWidth::H16 if shape.signed => self
                        .buffer
                        .emit(Arm64Instruction::Sxth { rd: addr, rn: addr }),
                    // `caload`'s zero-extended halfword, and `iaload`'s word,
                    // are already the value; `SXTW` below canonicalizes both.
                    _ => {}
                }
                self.emit_sxtw(addr);
                self.push_reg(OperandKind::I32, addr);
            }
            OperandKind::F32 => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFpSingle { vd: v, rn: addr });
                self.push_reg(OperandKind::F32, v);
            }
            OperandKind::F64 => {
                let v = self.alloc_reg(true);
                self.buffer
                    .emit(Arm64Instruction::FmovToFp { vd: v, rn: addr });
                self.push_reg(OperandKind::F64, v);
            }
            // `I64` (`laload`), the only other kind the table produces.
            kind => self.push_reg(kind, addr),
        }
        true
    }

    /// `iastore`/`lastore`/`fastore`/`dastore`/`bastore`/`castore`/`sastore`
    /// (round 9 wave 13). `aastore` is not here: see
    /// [`array_element_store_shape`].
    ///
    /// Guards, the `bastore` boolean mask, then one store of the element's own
    /// width -- `STRB`/`STRH` truncate, which is exactly the narrowing JVMS
    /// specifies for `bastore`/`castore`/`sastore`.
    pub(in crate::aarch64_backend) fn emit_array_store(&mut self, opcode: u8) -> bool {
        let Some(shape) = array_element_store_shape(opcode) else {
            return false;
        };
        if !self.can_access_array() {
            return false;
        }
        let val = self.pop_kind(shape.kind);
        let idx = self.pop_kind(OperandKind::I32);
        let arr = self.pop_kind(OperandKind::Ref);
        let Some(addr) = self.emit_array_guards_and_address(arr, idx, shape.action, shape.shift)
        else {
            return false;
        };
        // An FP value moves into a GPR bit-exactly first: `MemStore` names a
        // general register, and the bits are what the element holds.
        let src = match shape.kind {
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
        if opcode == 0x54 {
            self.emit_bastore_boolean_mask(arr, src);
        }
        self.buffer.emit(Arm64Instruction::MemStore {
            rt: src,
            rn: addr,
            width: shape.width,
            release: false,
        });
        true
    }

    /// Call a runtime helper that CAN SAFEPOINT, with an oop map at the return
    /// address (round 9 wave 18).
    ///
    /// ```text
    ///     <store every register-located operand to its depth slot>
    ///     <store every register-homed reference LOCAL to its safepoint home>
    ///     <stamp the safepoint id>
    ///     MOV       X0..X7, <arguments>
    ///     MOVZ/MOVK X16, #helper
    ///     BLR       X16
    ///     <oop map recorded at the return address>
    ///     MOV       Xinto, X0
    ///     <reload the locals, then the operands>
    /// ```
    ///
    /// # What this adds to [`Self::emit_helper_call`], and why it is a second
    /// function rather than a flag
    ///
    /// `emit_helper_call` records no map, and says so as an obligation each
    /// caller discharges for ITS helper: `jit_putfield_*`, `jit_getfield` and
    /// `jit_putstatic_*` each have a specific argument for why they cannot
    /// safepoint. `jit_new_object` and friends have no such argument — they
    /// allocate, which can trigger a collection, which can MOVE every object
    /// this frame holds. Three things follow, and none of them is a flag on
    /// the existing sequence:
    ///
    /// * the reference operands must be in their depth slots AND NAMED, so the
    ///   collector rewrites them rather than merely marking them;
    /// * the reference LOCALS must be out of X19-X28 and in their safepoint
    ///   homes, for the same reason — a callee-saved register survives the
    ///   call, but inside the CALLEE's save area, where only a conservative
    ///   walk sees it, and a conservative walk cannot rewrite a relocated
    ///   object;
    /// * the reload afterwards is not housekeeping. It is what carries the
    ///   moved object's new address back into the register the model says it
    ///   is in.
    ///
    /// That is exactly the loop-header poll's sequence, and it is literally
    /// the same code: both call [`Self::spill_operands_for_call`],
    /// [`Self::home_reference_locals_for_call`], [`Self::stamp_safepoint_id`]
    /// and [`Self::reload_after_safepoint`].
    ///
    /// # What the caller still owes
    ///
    /// * **`into` is allocated by the caller, before any branch**, exactly as
    ///   for `emit_helper_call` — and this refuses one that still holds a live
    ///   operand, for the same reason.
    /// * **No reference may be live only in an ARGUMENT register.** X0-X7 are
    ///   outside the map, so a reference handed to the helper and needed after
    ///   it would come back stale if the collector moved it. Every caller here
    ///   passes only the context pointer (not a Java object), immediates, and
    ///   values it does not use again.
    /// * **The result is not covered either.** It lands in X0 and moves to
    ///   `into`, both outside the map — which is sound only because the map is
    ///   recorded at the return address, i.e. for the window in which the
    ///   helper runs, and by then the result is a brand-new object no other
    ///   thread can reach.
    ///
    /// Answers `false`, having set `self.failed`, when it refuses. A
    /// compilation whose frame reserved no safepoint homes refuses here rather
    /// than emitting a call with a map that cannot name the locals.
    pub(in crate::aarch64_backend) fn emit_helper_call_at_safepoint(
        &mut self,
        helper: usize,
        args: &[Arm64HelperArg],
        into: Option<Arm64Register>,
        safepoint_id: u32,
    ) -> bool {
        self.emit_helper_call_at_safepoint_consuming(helper, args, into, safepoint_id, 0)
    }

    /// [`Self::emit_helper_call_at_safepoint`] for a call that CONSUMES the
    /// top `consumed` operand entries (round 9 wave 22).
    ///
    /// `invoke*` is the only caller, and it needs all three halves of this:
    ///
    /// * the arguments must be IN THE FRAME when the call happens, because the
    ///   helper reads them out of it -- which the spill does for every entry
    ///   the model holds, at each entry's own depth slot;
    /// * each reference among them must be NAMED, which the map does for the
    ///   same reason and by the same route, so a collector that moves one
    ///   during the callee hands the helper its new address;
    /// * and none of them may be RELOADED afterwards, because they are gone.
    ///   Reloading `n` dead values would not be merely wasteful: `invoke*` can
    ///   consume seven entries, which is the whole scratch pool, and the
    ///   reload would restore them over the registers the result needs.
    ///
    /// So the entries are dropped from the model after the map is recorded and
    /// before the reload, and the reload list is truncated to the survivors.
    /// `stored` is built in ascending depth order, so the survivors are
    /// exactly its prefix -- `keep` is counted before the spill for that
    /// reason, from the depths that will still exist.
    pub(in crate::aarch64_backend) fn emit_helper_call_at_safepoint_consuming(
        &mut self,
        helper: usize,
        args: &[Arm64HelperArg],
        into: Option<Arm64Register>,
        safepoint_id: u32,
        consumed: usize,
    ) -> bool {
        if helper == 0 {
            self.failed = true;
            return false;
        }
        if args.len() > Arm64EntryConvention::INT_ARG_REGS.len() {
            self.failed = true;
            return false;
        }
        for arg in args {
            if let Arm64HelperArg::Reg(reg) = *arg {
                if !SCRATCH_REGS.contains(&reg) {
                    self.failed = true;
                    return false;
                }
            }
        }
        if let Some(dst) = into {
            if !SCRATCH_REGS.contains(&dst) || self.reg_is_live(dst) {
                self.failed = true;
                return false;
            }
        }
        if self.sp_id_slot_off == 0 {
            // No id word means `active_safepoint_id` answers `None` at run
            // time and no map can be selected, so the map this call is about
            // to record would be unreachable. The frame reserves the word for
            // any method that allocates; reaching here means it did not.
            self.failed = true;
            return false;
        }

        let Some(surviving) = self.operand_stack.len().checked_sub(consumed) else {
            self.failed = true;
            return false;
        };
        // Counted BEFORE the spill, over the depths that outlive the call.
        let keep = (0..surviving)
            .filter(|&d| matches!(self.operand_stack[d].loc, OperandLoc::Reg(_)))
            .count();
        let Some(mut stored) = self.spill_operands_for_call() else {
            return false;
        };
        let Some(reg_homed) = self.home_reference_locals_for_call(false) else {
            return false;
        };
        self.stamp_safepoint_id(safepoint_id);

        for (i, arg) in args.iter().enumerate() {
            let dst = Arm64EntryConvention::INT_ARG_REGS[i];
            match *arg {
                Arm64HelperArg::Reg(src) => {
                    self.buffer.emit(Arm64Instruction::Mov { rd: dst, rm: src });
                }
                Arm64HelperArg::Imm(imm) => {
                    self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm });
                }
            }
        }
        // Cast: a helper address is a real mapped pointer, always < i64::MAX.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: helper as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });
        self.emit_oop_map_for_safepoint(safepoint_id);
        if self.failed {
            return false;
        }
        if let Some(dst) = into {
            self.buffer.emit(Arm64Instruction::Mov {
                rd: dst,
                rm: Arm64Register::X0,
            });
        }
        // The consumed entries die HERE: after the map named them (they were
        // live for the whole call) and before the reload would resurrect them.
        if consumed > 0 {
            self.operand_stack.truncate(surviving);
            stored.truncate(keep);
        }
        self.reload_after_safepoint(&reg_homed, stored);
        true
    }

    // -- Instance fields ----------------------------------------------------

    /// JVMS §6.5 `putfield` narrowing, for a field whose declared type is
    /// narrower than the `int` on the operand stack.
    ///
    /// Exactly x64's `emit_narrow_to_field_tag`, instruction for instruction:
    /// `B` and `S` sign-extend their low 8 / 16 bits, `C` keeps its low 16
    /// unsigned, and `Z` keeps one bit. `I`, `J`, `F` and `D` owe nothing.
    ///
    /// javac already narrows (`i2b`, `i2s`, `i2c`, a 0/1 `boolean`), so for
    /// javac output every one of these is a no-op ON THE VALUE -- but the
    /// helper writes a whole `Value::Int` into a legacy cell, and a
    /// handwritten or rewritten class file that skips the conversion would
    /// otherwise store a `byte` of 300.
    pub(in crate::aarch64_backend) fn emit_narrow_to_field_tag(&mut self, reg: Arm64Register, type_tag: u8) {
        let inst = match type_tag {
            b'B' => Arm64Instruction::Sxtb { rd: reg, rn: reg },
            b'S' => Arm64Instruction::Sxth { rd: reg, rn: reg },
            b'C' => Arm64Instruction::AndImm {
                rd: reg,
                rn: reg,
                imm: 0xFFFF,
            },
            b'Z' => Arm64Instruction::AndImm {
                rd: reg,
                rn: reg,
                imm: 1,
            },
            _ => return,
        };
        self.buffer.emit(inst);
    }

}
