#[allow(unused_imports)]
use super::*;

impl Compiler {
    /// Lower `invokedynamic` at `pc`.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::x64::op_invoke) fn walk_invokedynamic(
        &mut self,
        _code: &[u8],
        _code_len: usize,
        op: u8,
        mut pc: usize,
        _dead: &mut bool,
        _branch_targets: &[bool],
    ) -> WalkStep {
        match op {
            // invokedynamic — unconditional deopt to the interpreter.
            //
            // This instruction is never actually JIT-executed: rather than
            // building call-site machinery for MethodHandle/CallSite
            // dispatch, the codegen jumps straight to the EXISTING shared
            // uncommon-trap deopt stub (reason 8 = `UnreachedCode`), whose
            // pre-existing policy (`DeoptimizationController::recommend_action`,
            // `jit/src/deopt.rs`) gives up immediately on first occurrence —
            // exactly the right fail-safe: if this exact program point is
            // ever actually reached at runtime (assertions enabled, or a
            // genuinely live indy), the method permanently reverts to
            // interpreter-only execution for the rest of the process (i.e.
            // today's status quo for that one method), while the
            // overwhelmingly common case — a dead `assert cond : "msg" +
            // var;` branch — never takes the trap and the surrounding hot
            // method compiles and runs at full JIT speed.
            //
            // Only the STACK EFFECT is modeled here (pop the call's args,
            // push a placeholder result of the correct kind) so the
            // compiler's simulated operand stack stays consistent for
            // whatever bytecode follows the (unreachable, but still
            // compiled) invokedynamic — e.g. the assert-message pattern's
            // `invokespecial AssertionError.<init>` + `athrow`.
            0xba => {
                // O(1) pc-indexed lookup — see `indy_info` field doc.
                let info = self
                    .indy_info_idx
                    .get(&pc)
                    .map(|&i| self.indy_info[i].clone());
                let Some((_pc, arg_slots, ret_type, arg_type_tags, bridge_site)) = info else {
                    // No resolver, or this site couldn't be resolved at
                    // compile time: fail safe and bail the whole method,
                    // exactly like every other CP-resolved metadata miss
                    // in this backend (see 0x12/0x13 above) — never emit
                    // unsound code for an unresolvable call site.
                    return WalkStep::Return(false);
                };

                self.flush_scratch_registers();

                // A BRIDGED site has a resolved, process-lifetime
                // handler, so call it directly instead of taking the
                // uncommon trap below. This is what removes RBC.7's premise
                // for the common `println("..." + x)`-after-a-loop shape:
                // with no trap at the indy bci there is nothing for an OSR
                // frame to resume imprecisely, so the OSR artifact keeps
                // running.
                //
                // The bridged set is `StringConcatFactory` and, since
                // 2026-08-23, `LambdaMetafactory`, `SwitchBootstraps` and
                // `ObjectMethods` — every bootstrap whose implementation
                // reaches the frame only through its operand stack and its
                // class id, which is all a bridge can offer. Between them
                // they cover a lambda or method reference, a
                // pattern-matching `switch`, and a record's
                // `equals`/`hashCode`/`toString`. The lambda one is what
                // lets a method that CREATES a lambda stay compiled at all:
                // before it, such a method took this trap on its first
                // execution and was retired with `MakeNotCompilable`, which
                // on Reactor/WebFlux assembly means essentially nothing is
                // ever compiled. The site pointer is self-describing (its
                // `kind` tag), so ONE call sequence and one entry serve
                // both. Every other bootstrap kind still falls through to
                // the trap.
                let bridge_entry = self.direct_helpers.indy_bridge;
                if indy_site_is_bridged(bridge_site, bridge_entry, ret_type) {
                    if false {
                        eprintln!("[cratonvm-jitc] indy bridge pc={} args={}", pc, arg_slots);
                    }
                    // nio21 wave 10, lane L3 (W9L2-1): a non-capturing lambda
                    // first reads its singleton through the site's constant
                    // word (see `emit_indy_constant_probe`); a hit IS the
                    // instruction and skips the whole bridge sequence.
                    let constant_hit =
                        (arg_slots == 0 && ret_type == b'L' && indy_constant_lambda_enabled())
                            .then(|| self.emit_indy_constant_probe(bridge_site));
                    let pre_pop_spill = self.next_spill_offset;
                    // `pop_invoke_args`, not a bare `pop_stack` loop: it
                    // also hands back the per-argument OOP MARKS, and the
                    // staged buffer below is the only thing holding those
                    // references across a call that runs a bootstrap and
                    // allocates. Without `pending_staged_arg_oops` the
                    // safepoint map does not name them, which for a
                    // capturing lambda means every captured object is
                    // invisible to a collection that happens inside its own
                    // creation. The concat bridge this arm grew out of had
                    // the same gap and never showed it, because a
                    // `StringConcatFactory` argument is read into a Rust
                    // `String` before anything can allocate.
                    let (arg_slots_vec, arg_oops) = self.pop_invoke_args(arg_slots);
                    let post_pop_spill = self.next_spill_offset;
                    if arg_slots > 0 {
                        let Some(args_end) = self.checked_spill_range_end(pre_pop_spill, arg_slots)
                        else {
                            if false {
                                eprintln!(
                                    "[cratonvm-jitc] indy bridge spill overflow pc={} base={} args={}",
                                    pc, pre_pop_spill, arg_slots
                                );
                            }
                            return WalkStep::Return(false);
                        };
                        self.next_spill_offset = args_end;
                        for (i, slot) in arg_slots_vec.iter().enumerate() {
                            let offset = pre_pop_spill + ((arg_slots - 1 - i) as i32) * 8;
                            self.load_slot_to_reg(RAX, *slot);
                            self.emit_store_local(offset, RAX);
                            if arg_oops[i] {
                                self.pending_staged_arg_oops.push(offset);
                            }
                        }
                        // gci i2/sp (`CRATONVM_JIT_SP_ZERO_DEAD_ARG_SOURCES`,
                        // default ON): as the dispatch arms -- the buffer
                        // starts at the pre-pop cursor, the bridge and the
                        // inline concat read only the buffer, and a failing
                        // bridge leaves through the post-pop exception check,
                        // so the popped sources are dead copies now.
                        self.emit_zero_dead_arg_sources(
                            &arg_slots_vec,
                            &arg_oops,
                            post_pop_spill,
                            pre_pop_spill,
                        );
                    }
                    // JIT round 19 proposal wave 4, lane strgap4 (STRGAP19-3,
                    // `CRATONVM_JIT_CONCAT_INLINE`, default ON, `=0` off): an admitted
                    // concat site is built inline from the staged operands;
                    // every decline lands on the unchanged bridge call below,
                    // and the fast path joins after its exception check with
                    // RAX = the String. Nothing is emitted when it is off.
                    let mut concat_decline: Vec<usize> = Vec::new();
                    let concat_done = self.try_emit_concat_inline(
                        pc,
                        bridge_site,
                        pre_pop_spill,
                        arg_slots,
                        ret_type,
                        &mut concat_decline,
                    );
                    for p in concat_decline {
                        self.patch_rel32_to_here(p);
                    }
                    self.emit_load_vm_context(ARG_REGS[0]);
                    self.emit_mov_imm64(ARG_REGS[1], bridge_site as i64);
                    if arg_slots > 0 {
                        self.emit_lea_frame_slot(
                            ARG_REGS[2],
                            pre_pop_spill + ((arg_slots as i32) - 1) * 8,
                        );
                    } else {
                        self.emit_xor_reg_self(ARG_REGS[2]);
                    }
                    self.emit_mov_imm32_sx(ARG_REGS[3], arg_slots as i32);
                    // j27 wave 7, lane T1DISP20 (IN16-3,
                    // `CRATONVM_JIT_SP_INDY_BRIDGE_SPILL_PUBLISHED`, opt-in):
                    // the dispatch arm's published spill. Every argument is in
                    // the staged buffer (its oops named by
                    // `pending_staged_arg_oops`), ARG_REGS carry the bridge ABI
                    // (vm/site/buf/count), never a Java oop, and RAX is published
                    // only when the staging loop -- which writes through RAX --
                    // ran (that spill claims RAX dead at the call too). Off,
                    // dev 9936c1a7a6's helper-call spill (RAX dead under
                    // `CRATONVM_JIT_RAX_DEAD_AT_CALL`).
                    if sp_indy_bridge_spill_published_enabled() {
                        self.emit_pre_safepoint_spill_args_published(true, arg_slots > 0);
                    } else {
                        self.emit_pre_safepoint_spill_before_helper_call();
                    }
                    self.emit_call_absolute(bridge_entry);
                    self.emit_oop_map_for_safepoint();
                    self.next_spill_offset = post_pop_spill;
                    // The bridge is FALLIBLE, and the `LambdaMetafactory`
                    // half is fallible in a way the concat half is not: a
                    // bootstrap can raise `BootstrapMethodError` /
                    // `LambdaConversionError`, and the VM-side entry stashes
                    // that and returns the `i64::MIN` deopt sentinel. Route
                    // it through the same shared stub every other fallible
                    // helper call uses; without it the sentinel bits would
                    // be pushed AS AN OBJECT REFERENCE and dereferenced by
                    // whatever consumes the result.
                    //
                    // Added with the generic bridge rather than before it
                    // because the concat entry's only failure answer is `0`,
                    // i.e. a null `String` — wrong, but not a wild pointer.
                    self.emit_post_invoke_exception_check(ret_type);
                    if let Some(hit) = constant_hit {
                        // Both arms hold the reference in RAX here. `done` is a
                        // join: no reload elision may pair a store emitted on
                        // the bridge arm with a load after it (as the `ldc`
                        // slot site, `objects.rs` `emit_ldc_cp_site`).
                        self.patch_rel32_to_here(hit);
                        self.slot_mirror = None;
                    }
                    // STRGAP19-3: the inline concat's join, RAX = the String
                    // on both arms. A join as above, so no reload elision
                    // across it either.
                    if !concat_done.is_empty() {
                        for done in concat_done {
                            self.patch_rel32_to_here(done);
                        }
                        self.slot_mirror = None;
                    }
                    // Pushed by the DESCRIPTOR, the same three-way choice
                    // the invoke lowering above makes: `xmm0` for `D`/`F`,
                    // a plain slot otherwise, oop-marked for `L`/`[`. A
                    // `V` site never reaches here — it is refused at
                    // admission, because a void bridge has no push for
                    // this to model.
                    if matches!(ret_type, b'D' | b'F') {
                        self.push_from_rax_as_xmm0();
                    } else {
                        self.push_from_rax();
                    }
                    if matches!(ret_type, b'L' | b'[') {
                        self.mark_top_as_oop();
                    }
                    pc += 5;
                    return WalkStep::Next(pc);
                }

                // Unconditional JMP to the shared deopt stub. Mirrors the
                // conditional String-intrinsic bail edges elsewhere in
                // this file (`emit_jcc_rel32_patch` + `deopt_stubs.push`),
                // but unconditional (`emit_jmp_rel32_patch`) since this
                // instruction is NEVER taken on the JIT-compiled path.
                // FIX (silent data corruption, HHH-15895 InPredicateTest /
                // AccumRepro3 residual): the fallback for this trap when no
                // precise snapshot exists ("safe reject" in the VM's
                // `try_osr()`) can only rewind execution to the method's OSR
                // entry bci, discarding every side effect committed by
                // JIT-compiled code between OSR entry and this trap — this
                // instruction can be reached arbitrarily late in a method
                // (e.g. inside a `println` well after earlier loops/mutations
                // already ran to completion), so "rewind to entry" silently
                // re-executes or drops already-committed work. Unlike the
                // experimental speculative-guard snapshots elsewhere in this
                // file (gated behind `deopt_real_enabled()`), this one is
                // unconditional: `emit_deopt_stubs` always uses it for reason
                // 8 (see the matching fix note there), because the imprecise
                // fallback here has PROVEN silent corruption risk, not just
                // performance cost. Reuses the existing OSR-exit snapshot
                // machinery (frame reconstruction from live registers/spill
                // slots) so the VM can resume precisely at THIS bci instead of
                // rewinding. Tagged `UnreachedCode` (the trap's true
                // reason) so the resume sinks' de-speculation applies the
                // give-up-immediately policy — see
                // `emit_osr_exit_map_at_reason`.
                //
                // deopt-osr indy-arg-types fix: record this call's own
                // per-argument type tags BEFORE the snapshot is built, so
                // the operand-stack loop can precisely type the top
                // `arg_type_tags.len()` stack entries instead of falling
                // back to the coarse `wide_fp` gate — see
                // `indy_stack_arg_types`'s doc comment.
                //
                // j27 wave 4 (lane INDYTRAP): the twin of the `indy bridge
                // pc=` line above, so one `CRATONVM_DBG_JITC=1` run names
                // every reason-8 trap a body plants (`has_indy_trap`) and why
                // (`site=0`: the VM built no bridge site; else no helper or a
                // `void` site).
                if false {
                    eprintln!(
                        "[cratonvm-jitc] indy trap pc={pc} args={arg_slots} ret={} site={bridge_site:#x} in {}",
                        ret_type as char,
                        self.method_label
                    );
                }
                if !arg_type_tags.is_empty() {
                    self.indy_stack_arg_types.insert(pc, arg_type_tags.clone());
                }
                self.emit_osr_exit_map_at_reason(pc, crate::deopt::DeoptReason::UnreachedCode);

                // This trap is UNCONDITIONAL: every execution of this bci
                // deopts. So if the snapshot just built cannot be
                // materialised back into an interpreter frame, the method
                // is guaranteed to fail on its first compiled call --
                // `build_deopt_frame_inner` returns `None` and the resume
                // sink refuses with `precise deoptimization unavailable
                // ... refusing side-effecting replay`, a hard
                // `InternalError` rather than a slow path.
                //
                // The usual producer of an unmaterialisable slot here is
                // the coarse `wide_fp` gate in the snapshot's operand-stack
                // loop: in a method that touches any long/float/double, a
                // non-oop stack entry that is NOT one of this call site own
                // arguments has no per-entry width source and is recorded
                // `Unsupported`. javac `ClassReader.readInnerClasses` is
                // the canonical shape -- `optPoolEntry(int, IntFunction,
                // Object)` leaves an `int` underneath the lambda argument,
                // so the indy-arg tags type the top entry but not that one.
                //
                // Compiling such a method is strictly worse than
                // interpreting it, so bail the whole compile. This is what
                // the per-method SPRING-TESTCOMPILER / HIB-STOREDPROC-JIT
                // bans did by hand for the javac family; deciding it from
                // the snapshot itself covers every method with this shape
                // rather than the ones somebody happened to hit.
                let unresumable_trap = self
                    .deopt_points
                    .last()
                    .is_some_and(|p| !crate::deopt::frame_state_is_resumable(&p.frame_state));
                if unresumable_trap {
                    if false {
                        eprintln!("[cratonvm-jitc] compile-bail unresumable-indy-trap bci={pc}");
                    }
                    self.buf.mark_codegen_unencodable("unresumable-indy-trap");
                }

                let patch = self.emit_jmp_rel32_patch();
                self.deopt_stubs.push((patch, pc, 8)); // 8 = DEOPT_REASON_UNREACHED_CODE

                // Stack-effect-only bookkeeping for subsequent (unreachable
                // but still-compiled) bytecode: pop the call's arguments —
                // popping is type-agnostic, so only the count matters —
                // then push a single placeholder of the correct STACK-SLOT
                // KIND for the descriptor's return type (control never
                // reaches past the trap above, so the placeholder's actual
                // bit-pattern is irrelevant; only its kind must match what
                // downstream codegen expects).
                for _ in 0..arg_slots {
                    self.pop_stack();
                }
                match ret_type {
                    b'V' => {}
                    b'F' | b'D' => {
                        self.emit_xor_reg_self(RAX);
                        self.push_from_rax_as_xmm0();
                    }
                    b'L' | b'[' => {
                        self.emit_xor_reg_self(RAX);
                        self.push_from_rax();
                        self.mark_top_as_oop();
                    }
                    _ => {
                        // int / long / short / byte / char / boolean
                        self.emit_xor_reg_self(RAX);
                        self.push_from_rax();
                    }
                }
                pc += 5;
            }
            _ => {
                self.fail("singlepass-codegen/walk-family-misdispatch");
                return WalkStep::Return(false);
            }
        }
        WalkStep::Next(pc)
    }

    /// The slow half of the inline self-call stack check, before the guard
    /// helper: if this frame's floor slot still holds the OSR trampoline's
    /// `usize::MAX` placeholder, fetch the real floor with the leaf
    /// `native_stack_floor_fn` and store it. The guard helper then runs as
    /// before, so this crossing is checked exactly as it always was; what
    /// changes is that every LATER self-call of the frame (and every child that
    /// inherits the slot) passes the inline `CMP RSP, [rbp - floor]; JA` fast
    /// path instead of paying the helper, a blind spill and an oop map each
    /// time. The trampoline cannot store the real floor itself: it is cached
    /// per target across threads and has no helper table (`osr_entry.rs`).
    /// Page `r11-x64gen-osr-frame-stack-floor-placeholder`.
    ///
    /// Emitted AFTER the caller's safepoint spill and BEFORE the guard CALL, so
    /// the leaf call runs in exactly the register state the guard call does. It
    /// deliberately does NOT branch to the fast path's merge: the spill may
    /// have pushed shadow-stack entries that only the guard's oop map reloads.
    ///
    /// ```text
    ///   cmp qword [rbp-floor], -1 ; jne .guard
    ///   call native_stack_floor_fn ; mov [rbp-floor], rax
    /// .guard:
    /// ```
    pub(in crate::x64) fn emit_osr_placeholder_floor_refresh(&mut self) {
        if self.stack_floor_slot_off == 0 || self.helpers.native_stack_floor_fn == 0 {
            return;
        }
        // CMP qword [rbp - floor], imm8(-1)  (REX.W 83 /7 ib)
        self.buf.emit_byte(0x48);
        self.buf.emit_byte(0x83);
        self.modrm_rbp_disp(7, self.stack_floor_slot_off);
        self.buf.emit_byte(0xFF);
        let real_floor = self.emit_jcc_rel32_patch(0x85); // JNE .guard
                                                          // () -> floor, in RAX. A leaf: touches no VM state, never GCs.
        cratonvm_jit_api::assert_helper_call_shape!(
            "native_stack_floor_fn",
            int_args = 0,
            returns_value = true
        );
        self.emit_call_absolute(self.helpers.native_stack_floor_fn);
        self.emit_store_local(self.stack_floor_slot_off, RAX);
        self.patch_rel32_to_here(real_floor);
    }

    /// Round 9 wave 7 (lane `mutrec7`): lower the CLOSING edge of a
    /// compile-time recursion cycle as a guarded call through its
    /// [`crate::JitCycleEdgeCell`] (see that type for the design and the
    /// soundness argument, and
    /// `perf-mutual-recursion-forces-every-call-through-invoke-dispatch-20260918.md`).
    ///
    /// ```text
    ///   [cmp rsp,[rbp-floor] ; ja .ok]              ; only with a floor slot
    ///   <spill> mov ARG0,[rbp-heap] ; call self_call_stack_guard ; <map>
    ///   test rax,rax ; jnz .exc                     ; i64::MIN = SOE stashed
    /// .ok:
    ///   <pop args ; copy them to the service range> ; = dispatch args buffer
    ///   <direct-entry ABI staging, context in ARG0> ; <spill>
    ///   mov r11,cell ; mov r11,[r11] ; test r11,r11 ; jnz .call
    ///   mov ARG0,[rbp-heap] ; mov ARG1,info ; lea ARG2,[args] ; mov ARG3,n
    ///   mov r11,jit_invoke_dispatch
    /// .call:
    ///   call r11                                    ; one return pc, one map
    ///   <republish rbp> <map> <stack-arg cleanup>
    ///   <sentinel fast skip> <callee-deopt service check>
    /// .exc:
    ///   <post-invoke exception check>
    /// ```
    ///
    /// * The guard runs BEFORE the arguments leave the simulated operand
    ///   stack, so its own safepoint map names them where they are.
    /// * The cell word is loaded ONCE, after every other instruction of the
    ///   staging, and called through the same register with no blocking point
    ///   in between: a thread that saw a body cannot be quiescent until that
    ///   body's frame exists, so the retirement queue cannot free it. A word
    ///   cleared to `0` in the meantime just selects the dispatch helper.
    /// * Both targets return to the same `call r11`, so the post-call sequence
    ///   (and its oop map) is shared. The dispatch helper ignores the staged
    ///   direct-entry registers and the outgoing stack-argument block (its
    ///   shadow space lies inside that block on Win64); its args buffer is the
    ///   service range, laid out exactly as the dispatch arm lays its own out.
    ///   A dispatch that returns the sentinel reaches the callee-deopt service
    ///   check exactly as a direct callee that threw does.
    pub(in crate::x64::op_invoke) fn emit_cycle_edge_cell_call(
        &mut self,
        pc: usize,
        cell_addr: usize,
        info: *const crate::JitInvokeInfo,
    ) -> bool {
        // SAFETY: `info` comes from `self.invoke_info`, whose boxes the caller
        // keeps alive for the whole compile.
        let (n, ret_type) = unsafe { ((*info).num_jit_args, (*info).return_type) };
        if crate::deopt_real_enabled() {
            self.snapshot_pre_intrinsic_call(pc, crate::deopt::DeoptReason::ReceiverTypeChanged);
        }

        // 1. Stack-depth guard -- the self-call one, byte for byte.
        let fast_skip = if self.stack_floor_slot_off != 0 {
            self.emit_cmp_r64_rbp_local(RSP, self.stack_floor_slot_off);
            Some(self.emit_jcc_rel32_patch(0x87)) // JA .ok
        } else {
            None
        };
        // gcki-ngr: the guard's map shares the call's safepoint id (`pc`), and
        // the scan unions the masks of one id, so a RAX bit here kept RAX live
        // for the call below as well -- `ArrayList.grow`'s stale `elementData`
        // copy at its `Arrays.copyOf` edge, pinning the old humongous span.
        self.emit_pre_safepoint_spill_before_helper_call();
        self.emit_osr_placeholder_floor_refresh();
        self.emit_load_vm_context(ARG_REGS[0]);
        self.emit_xor_reg_self(ARG_REGS[1]);
        cratonvm_jit_api::assert_helper_call_shape!(
            "self_call_stack_guard",
            int_args = 2,
            returns_value = true
        );
        self.emit_call_absolute(self.helpers.self_call_stack_guard);
        if self.precise_maps || self.shadow_enabled {
            self.emit_oop_map_for_safepoint();
        }
        self.emit_test_r64_r64(RAX);
        let overflow = self.emit_jcc_rel32_patch(0x85); // JNE .exc
        if let Some(skip) = fast_skip {
            self.patch_rel32_to_here(skip);
        }

        // 2. Arguments: into the service range (the dispatch fallback's args
        // buffer and the deopt service's copy), then the direct-entry ABI.
        let args_frame_top = self.next_spill_offset;
        let (arg_slots, arg_oops) = self.pop_invoke_args(n);
        let post_pop_spill = self.next_spill_offset;
        let Some(base) = self.reserve_direct_call_service_slots(args_frame_top, &arg_slots) else {
            self.fail("singlepass-codegen/cycle-edge-cell-service-slots");
            return false;
        };
        for (i, slot) in arg_slots.iter().enumerate() {
            self.load_slot_to_reg(R11, *slot);
            // Cast: argument index bounded by the operand stack; x86-64 displacement.
            let off = base + ((arg_slots.len() - 1 - i) as i32) * 8;
            self.emit_store_local(off, R11);
            // Named unconditionally, as the dispatch arm names its buffer: the
            // range is live across the call on BOTH targets.
            if arg_oops.get(i).copied().unwrap_or(false) {
                self.pending_staged_arg_oops.push(off);
            }
        }
        let total_sub = self.emit_stack_arg_setup(&arg_slots, true);
        // gci i2/sp (`CRATONVM_JIT_SP_ZERO_DEAD_ARG_SOURCES`, default ON): the
        // service copy and the setup above were the popped sources' last
        // readers -- both targets read the service range (the dispatch
        // fallback's buffer, the callee-deopt service's copy) -- and the
        // pre-pop `ReceiverTypeChanged` point snapshotted above is baked by no
        // stub after the pop, so the reclaimed source words are zeroed as in
        // the ordinary direct arms.
        self.emit_zero_dead_arg_sources(&arg_slots, &arg_oops, post_pop_spill, args_frame_top);
        // Every Java argument is in the named service range and ARG_REGS carry
        // only copies of them (plus the context); RAX may hold a stack-argument
        // copy, so it stays in the spill.
        self.emit_pre_safepoint_spill_args_published(true, false);

        // 3. The target: the cell's entry, else the dispatch helper.
        self.emit_mov_imm64(R11, cell_addr as i64); // Cast: cell address as imm64
        self.emit_mov_r64_mem_disp32(R11, R11, crate::JIT_CYCLE_EDGE_CELL_ENTRY_OFFSET);
        self.emit_test_r64_r64(R11);
        let resolved = self.emit_jcc_rel32_patch(0x85); // JNZ .call
        self.emit_load_vm_context(ARG_REGS[0]);
        self.emit_mov_imm64(ARG_REGS[1], info as i64); // Cast: pointer as imm64
        if n > 0 {
            // Lowest address = arg 0, exactly the dispatch arm's buffer start.
            let buf_start = base + ((n as i32) - 1) * 8; // Cast: x86-64 displacement
            self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
        } else {
            self.emit_xor_reg_self(ARG_REGS[2]);
        }
        self.emit_mov_imm32_sx(ARG_REGS[3], n as i32); // Cast: x86-64 immediate
                                                       // (vm_ptr, info_ptr, args_ptr, num_args) -> result | sentinel.
        cratonvm_jit_api::assert_helper_call_shape!(
            "invoke_dispatch",
            int_args = 4,
            returns_value = true
        );
        self.emit_mov_imm64(R11, self.helpers.invoke_dispatch as i64); // Cast: fn address as imm64
        self.patch_rel32_to_here(resolved);
        // CALL R11 (REX.B + FF /2 + ModRM(11,/2,R11)), as the inline PIC does.
        self.buf.emit(&[0x41, 0xFF, 0xD3]);
        // gkr (OOM21-2): after the CALL the range is the service's alone; on
        // the dispatch target the helper read it before its callee ran, and
        // no compiled callee frame sits under this one.
        let ret = self.buf.pos();
        self.note_service_arg_copies(ret, base, &arg_oops);
        self.emit_post_call_rbp_republish();
        self.emit_oop_map_for_safepoint();
        self.emit_stack_arg_cleanup(total_sub);
        let merged_keep =
            merged_call_sentinel_enabled().then(|| self.emit_call_sentinel_fast_skip());
        self.emit_inline_callee_deopt_check(info, n, base);
        // The guard's overflow edge joins here with RAX = i64::MIN and its
        // stack balanced (it left before the stack-argument block existed).
        self.patch_rel32_to_here(overflow);
        self.emit_post_invoke_exception_check(ret_type);
        if let Some(keep) = merged_keep {
            self.patch_rel32_to_here(keep);
        }
        // See the direct-call arm: the result belongs at the popped depth, not
        // above the service range.
        self.next_spill_offset = post_pop_spill;
        if ret_type != b'V' {
            if matches!(ret_type, b'D' | b'F') {
                self.push_from_rax_as_xmm0();
            } else {
                self.push_from_rax();
            }
            if matches!(ret_type, b'L' | b'[') {
                self.mark_top_as_oop();
            }
        }
        true
    }

    /// Round 9 wave 9 (lane `evict9`): the CALL of a baked direct call made
    /// through its retire cell ([`crate::JIT_RETIRE_CELL_GUARD`],
    /// `evicted-callee-stays-reachable-through-a-compiled-callers-binding-FIXED-20260918.md`).
    ///
    /// Emitted by the ordinary direct-call arm in place of its
    /// `emit_call_absolute(callee_entry)`, AFTER the arguments are staged for
    /// the direct entry ABI, copied to the service range (`args_base`) and the
    /// pre-call spill / safepoint metadata is emitted:
    ///
    /// ```text
    ///   mov r11,cell ; mov r11,[r11] ; test r11,r11 ; jnz .call
    ///   mov ARG0,[rbp-heap] ; mov ARG1,info ; lea ARG2,[args] ; mov ARG3,n
    ///   mov r11,jit_invoke_dispatch
    /// .call:
    ///   call r11
    /// ```
    ///
    /// The caller's post-call sequence (republish, oop map, stack-argument
    /// cleanup, sentinel skip, callee-deopt service check, exception check) is
    /// shared by both targets, exactly as for the cycle-edge cell
    /// ([`Self::emit_cycle_edge_cell_call`], whose soundness notes apply):
    ///
    /// * The word is loaded ONCE, as the last thing before the `CALL`, with no
    ///   blocking point in between, so the body it names cannot be reclaimed
    ///   under this thread (the inline-cache argument).
    /// * The cell only ever holds its PINNED entry -- the one this code would
    ///   otherwise have baked -- or `0`, so the acyclicity of baked edges that
    ///   stack safety rests on is unchanged: no stack-depth guard is needed,
    ///   and a filled cell costs one load, a test and a taken branch.
    /// * The dispatch helper ignores the staged direct-entry registers and the
    ///   outgoing stack-argument block; its args buffer is the service range,
    ///   laid out as the dispatch arm lays out its own.
    pub(in crate::x64::op_invoke) fn emit_retire_cell_call_target(
        &mut self,
        cell_addr: usize,
        info: *const crate::JitInvokeInfo,
        n: usize,
        args_base: i32,
    ) {
        self.emit_mov_imm64(R11, cell_addr as i64); // Cast: cell address as imm64
        self.emit_mov_r64_mem_disp32(R11, R11, crate::JIT_CYCLE_EDGE_CELL_ENTRY_OFFSET);
        self.emit_test_r64_r64(R11);
        let resolved = self.emit_jcc_rel32_patch(0x85); // JNZ .call
        self.emit_load_vm_context(ARG_REGS[0]);
        self.emit_mov_imm64(ARG_REGS[1], info as i64); // Cast: pointer as imm64
        if n > 0 {
            // Lowest address = arg 0, exactly the dispatch arm's buffer start.
            let buf_start = args_base + ((n as i32) - 1) * 8; // Cast: x86-64 displacement
            self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
        } else {
            self.emit_xor_reg_self(ARG_REGS[2]);
        }
        self.emit_mov_imm32_sx(ARG_REGS[3], n as i32); // Cast: x86-64 immediate
                                                       // (vm_ptr, info_ptr, args_ptr, num_args) -> result | sentinel.
        cratonvm_jit_api::assert_helper_call_shape!(
            "invoke_dispatch",
            int_args = 4,
            returns_value = true
        );
        self.emit_mov_imm64(R11, self.helpers.invoke_dispatch as i64); // Cast: fn address as imm64
        self.patch_rel32_to_here(resolved);
        // CALL R11 (REX.B + FF /2 + ModRM(11,/2,R11)), as the cycle-edge cell does.
        self.buf.emit(&[0x41, 0xFF, 0xD3]);
    }

    /// The safepoint poll of a self-recursive TAIL call's back-edge.
    ///
    /// The tail form `JMP`s to `body_entry_offset`, which is taken AFTER the
    /// prologue (`driver.rs`), and the prologue's last act is the method-entry
    /// poll (`frames.rs`, `emit_safepoint_poll_prologue`). So the jump skipped
    /// the only poll a method whose sole loop is its own tail recursion has —
    /// `static int spin(int n) { return n == 0 ? 0 : spin(n - 1); }` ran
    /// `spin(Integer.MAX_VALUE)` as a poll-free loop and held off every
    /// stop-the-world for its whole length. HotSpot has no tail calls, so there
    /// the same method overflows the stack. Here it is a loop, and a loop polls.
    /// (`NOTES-invoke.md`, proposal 5.) The tail form itself is opt-in
    /// (`CRATONVM_JIT=self-tailcall`), so this is the poll of a configuration a
    /// user chose, not of the default one.
    ///
    /// # Why this is the ENTRY poll, re-emitted, and not a back-edge poll at `pc`
    ///
    /// By the time this runs, `emit_self_tail_param_moves` has put every
    /// argument in the parameter home the prologue would have put it in, and
    /// the jump target is the state right after that prologue. That is exactly
    /// the state the entry poll describes, and describes EXACTLY:
    /// `local_oop_mask_at_current_pc` answers `param_oop_mask` for
    /// `ENTRY_POLL_BC_PC` with an empty operand stack. The invoke's own pc is
    /// the wrong key: its `local_oop_masks` entry describes the locals BEFORE
    /// the moves, and a parameter slot the body reuses for a value of another
    /// type would be claimed (or not) by the old type while it holds the new
    /// argument.
    ///
    /// The prologue poll and this one therefore publish the same bytecode pc
    /// with the same claim — the parameter mask, nothing on the operand stack
    /// — which is why the operand stack must be EMPTY here. `ireturn` does not
    /// require it (values below the return value are discarded), so a
    /// hand-written tail site can leave some; it gets no poll, which is
    /// today's behaviour, rather than a second map under the entry key that
    /// disagrees with the first.
    ///
    /// # GC-inert methods
    ///
    /// A `gc_inert_selfrec` method has no entry poll at all and its raw
    /// self-CALLs publish nothing, so claiming moving-young coverage at a
    /// safepoint inside one would be a lie about the frames above it. It gets
    /// the protocol its native-stack guard already uses at the same site —
    /// `emit_pre_safepoint_spill_without_shadow`, then a map — under which a
    /// collection that lands there takes the non-moving fallback. That is the
    /// one method shape where this poll matters most: a spin like the one
    /// above is GC-inert (no allocation, no bytecode back-edge, no other call).
    ///
    /// Costs one `TEST BYTE [rip+flag]` + `JZ` per tail iteration. Disabled by
    /// the same switch as every other poll, `CRATONVM_JIT_SAFEPOINT_POLLS=0`.
    ///
    /// # Leaving for the interpreter (interpreter round i1 wave 15)
    ///
    /// The ordinary form reads the slow path's loop-exit verdict like a
    /// back-edge poll (`safepoint.rs::emit_self_tail_entry_poll`): the state
    /// here is bci 0 of the next activation, so an `OsrExit` map at bci 0
    /// describes it, and the interpreter resuming there is the same
    /// continuation. The GC-inert form does not leave. Its premise is that
    /// nothing below one of its activations reaches a moving collection: the
    /// raw self-`CALL` sites of its outer activations record no map. An exit
    /// would run the rest of the recursion in the interpreter below those
    /// activations (their callee-deopt service resumes the frame), where it
    /// can allocate and collect.
    pub(in crate::x64::op_invoke) fn emit_self_tail_safepoint_poll(&mut self) {
        if self.failed || !self.stack.is_empty() {
            return;
        }
        if !self.gc_inert_selfrec {
            // `emit_safepoint_poll_prologue`, plus (interpreter round i1
            // wave 15) the loop-exit verdict test when a map at bci 0
            // describes this state: swaps `cur_bc_pc` to `ENTRY_POLL_BC_PC`
            // and back, and applies the flag / helper-address gates itself.
            self.emit_self_tail_entry_poll();
            return;
        }
        self.emit_gc_inert_safepoint_poll();
    }

    /// The safepoint poll of a `gc_inert_selfrec` body: its method-entry poll
    /// (`frames.rs`, end of `emit_prologue`) and its self-tail back-edge poll
    /// ([`Self::emit_self_tail_safepoint_poll`]). Both stand at the state right
    /// after the prologue -- parameters homed, empty operand stack -- which is
    /// what `ENTRY_POLL_BC_PC` names.
    ///
    /// JIT round 20 wave 8 (lane huntMT,
    /// `jr20-gc-inert-self-recursion-never-polls-FIXED-20261012.md`), correctness,
    /// unconditional: the entry poll used to be OMITTED in such a body, on the
    /// premise that its recursive edge cannot reach a safepoint. That is true
    /// of collections and false of time: a body with two self-calls is an
    /// exponential recursion (`fib(50)`), so a thread in it never polled for
    /// minutes. A collection then froze it by OS take-over, but a frame-trace
    /// pause (`Thread.getStackTrace`, `getAllStackTraces`, thread dumps) froze
    /// it after its 2 ms grace and answered the trace the thread had last
    /// deposited -- HotSpot's top frame is the recursive method.
    ///
    /// The protocol is the cold one the overflow guard uses at the self-call
    /// site: `emit_pre_safepoint_spill_without_shadow`, the slow-path call,
    /// a map. The outer activations of the recursion record no map at their
    /// raw self-`CALL`s, so a collection that lands here takes the non-moving
    /// fallback, exactly as one landing in the overflow guard does. After the
    /// slow path the frame's safepoint-id slot is stamped `SP_ID_UNSET_BC_PC`
    /// again: the spill stored `ENTRY_POLL_BC_PC` there, and this activation
    /// goes on to make raw self-calls that store no id of their own, so a
    /// collection reached deeper down would otherwise find this frame's id
    /// naming the ENTRY map while the frame stands at a self-call. Unset, the
    /// coverage proof fails closed for it.
    pub(in crate::x64) fn emit_gc_inert_safepoint_poll(&mut self) {
        if self.failed || !self.stack.is_empty() {
            return;
        }
        if !jit_safepoint_polls_enabled()
            || self.helpers.safepoint_flag_addr == 0
            || self.helpers.safepoint_slow_path == 0
        {
            return;
        }
        let saved_pc = self.cur_bc_pc;
        self.cur_bc_pc = super::safepoint::ENTRY_POLL_BC_PC;
        // The flag test, exactly as `emit_safepoint_poll` emits it.
        if !jit_rip_safepoint_poll_enabled()
            || !self.emit_test_mem8_abs_imm8(self.helpers.safepoint_flag_addr, 0xFF)
        {
            self.emit_mov_imm64(R11, self.helpers.safepoint_flag_addr as i64); // Cast: x86-64 immediate encoding
            self.emit_test_mem8_imm8(R11, 0, 0xFF);
        }
        let no_poll = self.emit_jcc_rel32_patch(0x84); // JZ (flag clear) -> skip
        self.emit_pre_safepoint_spill_without_shadow();
        // Argument 0 = the flag byte this poll tested (names the VM).
        self.emit_mov_imm64(ARG_REGS[0], self.helpers.safepoint_flag_addr as i64); // Cast: x86-64 immediate encoding
                                                                                   // The helper's verdict (RAX) is not read in this GC-inert form (wave 15:
                                                                                   // see the doc above for why only the other form leaves).
                                                                                   // Argument 1 is defined all the same (wave 17).
        self.emit_safepoint_body_id_arg();
        cratonvm_jit_api::assert_helper_call_shape!(
            "safepoint_slow_path",
            int_args = 2,
            returns_value = true
        );
        self.emit_call_absolute(self.helpers.safepoint_slow_path);
        if self.precise_maps || self.shadow_enabled {
            self.emit_oop_map_for_safepoint();
        }
        // Back to "no safepoint reached" (see the doc above). RAX is dead: the
        // verdict is not read in this form, and the state here is bci 0 with
        // an empty operand stack.
        if self.precise_maps && self.sp_id_slot_off != 0 && crate::sp_id_slot_init_enabled() {
            // Cast: the sentinel is `u32::MAX - 1`, as the prologue stamps it.
            self.emit_mov_imm32_sx(RAX, super::safepoint::SP_ID_UNSET_BC_PC as u32 as i32);
            self.emit_store_local(self.sp_id_slot_off, RAX);
        }
        self.patch_rel32_to_here(no_poll);
        self.cur_bc_pc = saved_pc;
    }

    /// Move a self-recursive TAIL call's arguments into this method's own
    /// parameter homes, ahead of the `JMP` back to the body entry.
    ///
    /// The homes must be exactly the ones the prologue deposits incoming
    /// arguments in (`frames.rs`, `emit_prologue`), because the body the jump
    /// lands in reads them from there. This used to write argument `i` to local
    /// `i`'s GPR home or frame slot, which is right only for a method whose
    /// parameters are all category-1 and none of which has an XMM home:
    ///
    ///   * a `long`/`double` parameter spans TWO JVM local slots, so every
    ///     argument after it belongs one slot further on
    ///     (`param_jvm_slots`) — `f(long a, int b)` wrote `b` into `a`'s dead
    ///     high half and the body kept reading the OLD `b` from slot 2;
    ///   * a `float`/`double` parameter the allocator homed in an XMM register
    ///     is read by the body from that register, and the store went to a
    ///     frame slot nothing reads.
    ///
    /// It also moved the arguments one at a time, which is a PARALLEL-MOVE
    /// hazard the prologue never has (its sources are the ABI registers, which
    /// no parameter home aliases). Here a source can be a register-homed local
    /// — `return f(b, a);` pushes `CalleeSaved(home of b)` then
    /// `CalleeSaved(home of a)` — and writing parameter 0 destroys the source
    /// of parameter 1. When any later source reads a home an earlier move
    /// writes, every value is first staged on the machine stack (`PUSH` in
    /// order, `POP` in reverse): no call and no safepoint sits between them, so
    /// the transient `RSP` adjustment is invisible to the GC and to the frame
    /// layout. Without such an overlap the moves are emitted directly, which is
    /// byte-identical to the old sequence for a category-1, non-XMM signature.
    pub(in crate::x64::op_invoke) fn emit_self_tail_param_moves(&mut self, arg_slots: &[StackSlot]) {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Home {
            Gpr(u8),
            Xmm(u8),
            Frame(i32),
        }
        let homes: Vec<Home> = (0..arg_slots.len())
            .map(|i| {
                let slot = self.param_jvm_slots.get(i).copied().unwrap_or(i);
                if let Some(xmm) = self.xmm_for_local(slot) {
                    Home::Xmm(xmm)
                } else if let Some(reg) = self.reg_for_local(slot) {
                    Home::Gpr(reg)
                } else {
                    Home::Frame(self.local_offset(slot))
                }
            })
            .collect();
        let source_home = |slot: &StackSlot| -> Home {
            match *slot {
                StackSlot::Frame(off) => Home::Frame(off),
                StackSlot::CalleeSaved(reg) | StackSlot::Scratch(reg, ..) => Home::Gpr(reg),
                StackSlot::Xmm(xmm) => Home::Xmm(xmm),
            }
        };
        // Parameter `i` is written before the source of every `k > i` is read.
        let overlaps = arg_slots.iter().enumerate().any(|(k, slot)| {
            let src = source_home(slot);
            homes[..k].iter().any(|dst| *dst == src)
        });
        if overlaps {
            for slot in arg_slots {
                self.load_slot_to_reg(RAX, *slot);
                self.buf.emit_byte(0x50); // PUSH RAX
            }
            for i in (0..arg_slots.len()).rev() {
                self.buf.emit_byte(0x58); // POP RAX
                match homes[i] {
                    Home::Xmm(xmm) => self.emit_movq_xmm_from_rax(xmm),
                    Home::Gpr(reg) => self.emit_mov_reg_reg(reg, RAX),
                    Home::Frame(off) => self.emit_store_local(off, RAX),
                }
            }
            return;
        }
        for (i, slot) in arg_slots.iter().enumerate() {
            match homes[i] {
                Home::Gpr(reg) => self.load_slot_to_reg(reg, *slot),
                Home::Xmm(xmm) => {
                    self.load_slot_to_reg(RAX, *slot);
                    self.emit_movq_xmm_from_rax(xmm);
                }
                Home::Frame(off) => {
                    self.load_slot_to_reg(RAX, *slot);
                    self.emit_store_local(off, RAX);
                }
            }
        }
    }
}
/// Does this `invokedynamic` site lower to the BRIDGE call rather than to the
/// reason-8 uncommon trap?
///
/// The three terms the 0xba arm admits a bridged site on, in one place because
/// two callers now ask: the arm itself, and
/// [`Compiler::has_trapping_indy_site`], which the self-recursive
/// `invokestatic` arm consults before it bakes a raw `CALL` to this method's
/// own entry. A site needs a resolved bootstrap (`bridge_site`), the bridge
/// helper wired (`bridge_entry`), and a descriptor that returns a value —
/// `void` is refused because the arm's result push has nothing to model.
#[inline]
pub(crate) fn indy_site_is_bridged(bridge_site: usize, bridge_entry: usize, ret_type: u8) -> bool {
    bridge_site != 0 && bridge_entry != 0 && ret_type != b'V'
}

/// nio21 wave 10, lane L3: byte offset of the constant word in the VM's
/// `#[repr(C)]` indy site structs (`u32` kind, then the `usize` word). The VM
/// pins the same value with const assertions
/// (`vm::runtime::invokedynamic::JIT_INDY_SITE_CONSTANT_WORD_OFFSET`); the
/// optimizing twin is `ir_lower.rs`'s `Op::IndyBridge` arm.
pub(in crate::x64::op_invoke) const INDY_SITE_CONSTANT_WORD_OFFSET: i32 = 8;

/// `CRATONVM_JIT_INDY_CONSTANT_LAMBDA` (nio21 wave 10, lane L3; perf,
/// **default ON**): emit the constant-word probe in front of a non-capturing
/// lambda's bridge call. Read per site compile, so it adds no process global;
/// `=0` emits the wave-9 bridge sequence byte for byte (and the VM publishes
/// no word either).
pub(in crate::x64::op_invoke) fn indy_constant_lambda_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_INDY_BRIDGE_SPILL_PUBLISHED=1` (j27 wave 7, lane
/// T1DISP20, IN16-3; opt-in, unmeasured): the single-pass `invokedynamic`
/// bridge call takes `emit_pre_safepoint_spill_args_published(true, args > 0)`
/// -- the generic dispatch arm's spill for the same staged-buffer shape --
/// instead of `emit_pre_safepoint_spill_before_helper_call` (dev 9936c1a7a6;
/// before it, the blind spill). Read per site compile; no process global.
pub(in crate::x64::op_invoke) fn sp_indy_bridge_spill_published_enabled() -> bool {
    false
}







/// nio20 wave 3, lane L3 (W2L2-4): `CRATONVM_JIT_SP_THIN_PREFIX` (perf,
/// **default ON**; `0` keeps every single-pass `getClass()`, `Unsafe`
/// accessor, `Integer.intValue()` and `Long.longValue()` thin-bound site on
/// its call alone). The single-pass half of those prefixes only: the
/// optimizing tier's `getClass()` / `Unsafe` prefixes have their own lever
/// (`crate::ir_thin_prefix_enabled`), and `CRATONVM_JIT_GETCLASS_INLINE` /
/// `CRATONVM_JIT_UNSAFE_ACCESSOR_INLINE` still turn off both tiers' halves
/// together. The `valueOf` box rows keep their own switches
/// (`CRATONVM_JIT_SP_VALUEOF_INLINE_BOX`, `CRATONVM_JIT_LONG_VALUEOF_INLINE_BOX`).
/// Read per site compile, so it adds no process global.
pub(in crate::x64) fn sp_thin_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_VH_CONST_G1` (JIT round 21, lane spg1vh, G1VHSOFT21-1 of
/// `docs/feature-designs/jit-barrier-lowering.md`; page
/// `docs/feature-designs/jit-barrier-lowering.md`;
/// perf, **default ON**, `=0` keeps every single-pass reference `set*` / CAS /
/// `compareAndExchange*` / `getAndSet*` through a constant `VarHandle` on its
/// call under G1, as before): the single-pass constant-`VarHandle` prefix
/// builds a G1 reference write against G1's plan
/// (`Compiler::sp_vh_const_store_barriers`), with the post edge
/// (`Compiler::emit_sp_vh_g1_post_edge`). Read per site compile, so it adds
/// no process global.
pub(in crate::x64) fn sp_vh_const_g1_enabled() -> bool {
    true
}

/// `SUB RSP, 48 ; MOV [RSP + 32], RAX` (JIT round 21, lane spg1vh): the G1
/// post edge keeps the access's result above the barrier call's home area
/// (`Compiler::emit_sp_vh_g1_post_edge`).
pub(in crate::x64) const SP_VH_G1_KEEP_RESULT: [u8; 9] =
    [0x48, 0x83, 0xEC, 0x30, 0x48, 0x89, 0x44, 0x24, 0x20];

/// `MOV RAX, [RSP + 32] ; ADD RSP, 48`: [`SP_VH_G1_KEEP_RESULT`] undone.
pub(in crate::x64) const SP_VH_G1_RESTORE_RESULT: [u8; 9] =
    [0x48, 0x8B, 0x44, 0x24, 0x20, 0x48, 0x83, 0xC4, 0x30];

/// nio21 wave 11 L5 (W9L2-2): the run-time proof of
/// `Compiler::emit_sp_vh_const_prefix` that the handle operand (in R11) IS the
/// `static final` the plan was filed for: the static's current value, read the
/// way the inline `getstatic` reads a reference static (`objects.rs`
/// `try_emit_inline_getstatic`: the base cell, the statics block, the 64-bit
/// payload of the 16-byte cell), compared with it. Writes RCX and the flags.
/// Returns the bytes and the offset of the rel32 decline field, or `None` when
/// the cell's displacement does not fit a disp32.
///
/// ```text
///   MOV RCX, imm64 base_cell ; MOV RCX, [RCX] ; MOV RCX, [RCX + cell]
///   CMP RCX, R11 ; JNE decline
/// ```
pub(in crate::x64) fn sp_vh_const_handle_compare_bytes(
    base_cell: usize,
    field_index: usize,
) -> Option<(Vec<u8>, Vec<usize>)> {
    let cell = field_index
        .checked_mul(cratonvm_types::SLOT_SIZE)?
        .checked_add(cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET)?;
    let cell = i32::try_from(cell).ok()?;
    let mut b: Vec<u8> = Vec::with_capacity(32);
    b.extend_from_slice(&[0x48, 0xB9]); // MOV RCX, imm64
    b.extend_from_slice(&(base_cell as u64).to_le_bytes()); // Cast: the baked cell address
    b.extend_from_slice(&[0x48, 0x8B, 0x09]); // MOV RCX, [RCX]: the statics block
    b.extend_from_slice(&[0x48, 0x8B, 0x89]); // MOV RCX, [RCX + disp32]: the handle
    b.extend_from_slice(&cell.to_le_bytes());
    b.extend_from_slice(&[0x4C, 0x39, 0xD9]); // CMP RCX, R11
    b.extend_from_slice(&[0x0F, 0x85]); // JNE rel32: not the filed handle
    let edge = b.len();
    b.extend_from_slice(&[0, 0, 0, 0]);
    Some((b, vec![edge]))
}


/// `CRATONVM_JIT_SP_IDENTITY_HASH_INLINE` (nio21 wave 1, lane L5, **default
/// ON**, `=0` keeps every single-pass identity-hash site on its dispatch): the
/// single-pass half of the identity-hash header read
/// (`Compiler::try_emit_sp_identity_hash_dispatch_prefix`), so one binary can
/// A/B the tier alone; `CRATONVM_JIT_IR_IDENTITY_HASH_INLINE=0` turns off both
/// halves. Read per site compile, so it adds no process global.
pub(in crate::x64) fn sp_identity_hash_inline_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_BOX_HASH_CLASS_ARM` (nio21 wave 8, lane L5, **default
/// ON**, `=0` keeps a single-pass `Object.hashCode()` on a `java/lang/Class`
/// receiver on its dispatch): the `Class` arm of
/// `Compiler::try_emit_box_hash_equals_prefix`, the single-pass twin of the
/// optimizing tier's nio20 wave 5 arm. Also off under
/// `CRATONVM_JIT_SP_IDENTITY_HASH_INLINE=0` /
/// `CRATONVM_JIT_IR_IDENTITY_HASH_INLINE=0`. Read per site compile, so it adds
/// no process global.
pub(in crate::x64) fn sp_box_hash_class_arm_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_SCOPED_ACCESS_PREFIX` (nio21 wave 3, lane L3, proposal
/// W2L3-3; **default ON**, `=0` keeps every single-pass thin-bound
/// `ScopedMemoryAccess.{get,put}{Short,Char,Int,Long}Unaligned{,Internal}`
/// site on its helper call alone): the single-pass twin of the optimizing
/// tier's scoped fold, as a guarded prefix of that unchanged CALL
/// (`Compiler::try_emit_sp_scoped_access_prefix`). Also off under
/// `CRATONVM_JIT_SP_THIN_PREFIX=0`. Read per site compile, so it adds no
/// process global.
pub(in crate::x64) fn sp_scoped_access_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_UNSAFE_OFFHEAP_PREFIX` (nio21 wave 3, lane L3, proposal
/// W2L3-3; **default ON**, `=0` restores the nio20 prefix exactly: a null base
/// declines to the call): the single-pass `Unsafe` primitive-array prefix
/// (`Compiler::emit_sp_unsafe_access_core`) also serves a NULL base -- a raw
/// off-heap address, or an `Unsafe.allocateMemory` arena handle resolved
/// through the JIT-readable arena table (absent under
/// `CRATONVM_JIT_IR_SCOPED_ARENA_FOLD=0` / `CRATONVM_UNSAFE_ARENA_JIT_TABLE=0`,
/// when a handle declines). Read per site compile.
pub(in crate::x64) fn sp_unsafe_offheap_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_UNSAFE_BIG_ENDIAN_PREFIX` (JIT round 20, lane memory; page
/// `r20-unsafe-accessor-inline-is-optimizing-tier-only-FIXED-20261012.md`; **default
/// OFF** since round 20 wave 2, opt-in `=1`; off declines every four-operand
/// form as before. The JDK's callers never reach it: the four-operand forms
/// are `final` Java methods, so a single-pass site binds their compiled body
/// (or splices it) and never takes the dispatch arm, where this prefix sits;
/// the host A/B on `ZipUtils` showed no gain): the single-pass
/// `Unsafe` dispatch prefix (`Compiler::try_emit_sp_unsafe_dispatch_prefix`)
/// also serves `{get,put}{Short,Char,Int,Long}Unaligned(Object, long, [V,]
/// boolean bigEndian)`, swapping the bytes when the operand is non-zero, as
/// the optimizing tier's `Unsafe` prefix and the single-pass
/// `ScopedMemoryAccess` prefix already do. Read per site compile.
pub(in crate::x64) fn sp_unsafe_big_endian_prefix_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_SP_SCOPED_NEVER_CLOSING_SESSION` (nio21 wave 4, lane L3,
/// proposal W3L3-1; **default ON**, `=0` restores the wave-3 prefix exactly:
/// every non-null session declines to the call): the single-pass
/// `ScopedMemoryAccess` prefix (`Compiler::try_emit_sp_scoped_access_prefix`)
/// also serves a session whose exact class the optimizing fold admits as
/// never closing (`ir_lower::scoped_session_never_closing_class_ids`, itself
/// off under `CRATONVM_JIT_IR_SCOPED_SESSION_FOLD=0`). Read per site compile.
pub(in crate::x64) fn sp_scoped_never_closing_session_enabled() -> bool {
    true
}

/// The shape of a thin-bound `ScopedMemoryAccess` multi-byte accessor the
/// single-pass prefix serves (nio21 wave 3 L3): its width, whether a 2-byte
/// read sign-extends (`short`) or zero-extends (`char`), and whether it is a
/// put. Every one carries a trailing `bigEndian` operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::x64) struct SpScopedAccess {
    pub(in crate::x64) width: u8,
    pub(in crate::x64) signed: bool,
    pub(in crate::x64) is_put: bool,
}

impl SpScopedAccess {
    /// Declared parameters after the receiver: `(session, base, offset,
    /// [value,] bigEndian)`.
    pub(in crate::x64) const fn num_params(self) -> usize {
        if self.is_put {
            5
        } else {
            4
        }
    }

    /// The JVM return type tag.
    pub(in crate::x64) const fn ret_type(self) -> u8 {
        match (self.is_put, self.width, self.signed) {
            (true, _, _) => b'V',
            (false, 2, true) => b'S',
            (false, 2, false) => b'C',
            (false, 4, _) => b'I',
            _ => b'J',
        }
    }

    /// The `UnsafeInlineAccess::GATE_BITS` bit of the `Unsafe` capability
    /// gate the helper asks for this width (`scoped_memory_{read,write}_bytes`
    /// map 2 / 4 / 8 bytes to the `{get,put}{Short,Int,Long}` gates, `char`
    /// included).
    pub(in crate::x64) const fn unsafe_gate_bit(self) -> usize {
        let row = match self.width {
            2 => 1,
            4 => 3,
            _ => 4,
        };
        1usize << (row + if self.is_put { 5 } else { 0 })
    }
}

/// The [`SpScopedAccess`] of the thin helper `callee_entry` names: one of the
/// sixteen multi-byte `ScopedMemoryAccess` cells of `dh` (the public
/// `*Unaligned` binds and their `*Internal` twins). `None` for anything else,
/// an unwired `0` included. The byte accessors are left out: the helpers'
/// shared core declines every 1-byte access.
pub(in crate::x64) fn sp_scoped_access_of(
    dh: &crate::DirectHelperTable,
    callee_entry: usize,
) -> Option<SpScopedAccess> {
    if callee_entry == 0 {
        return None;
    }
    let get = |width: u8, signed: bool| SpScopedAccess {
        width,
        signed,
        is_put: false,
    };
    let put = |width: u8, signed: bool| SpScopedAccess {
        width,
        signed,
        is_put: true,
    };
    let rows = [
        (dh.scoped_memory_get_short, get(2, true)),
        (dh.scoped_memory_get_char, get(2, false)),
        (dh.scoped_memory_get_int, get(4, true)),
        (dh.scoped_memory_get_long, get(8, true)),
        (dh.scoped_memory_put_short, put(2, true)),
        (dh.scoped_memory_put_char, put(2, false)),
        (dh.scoped_memory_put_int, put(4, true)),
        (dh.scoped_memory_put_long, put(8, true)),
        (dh.scoped_memory_get_short_internal, get(2, true)),
        (dh.scoped_memory_get_char_internal, get(2, false)),
        (dh.scoped_memory_get_int_internal, get(4, true)),
        (dh.scoped_memory_get_long_internal, get(8, true)),
        (dh.scoped_memory_put_short_internal, put(2, true)),
        (dh.scoped_memory_put_char_internal, put(2, false)),
        (dh.scoped_memory_put_int_internal, put(4, true)),
        (dh.scoped_memory_put_long_internal, put(8, true)),
    ];
    rows.into_iter()
        .find(|&(cell, _)| cell == callee_entry)
        .map(|(_, access)| access)
}

/// nio20 wave 3, lane L3 (W2L2-1): the guarded prefixes a single-pass plain
/// direct arm may emit in front of its thin-helper CALL, one per
/// [`SP_THIN_PREFIX_ROWS`] family. Every one keeps the CALL unchanged behind
/// it and joins the result push on a hit (see
/// `Compiler::sp_thin_helper_prefix_admitted`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::x64) enum SpThinPrefix {
    /// `Integer.valueOf(I)`: an out-of-cache box from the TLAB (round 12).
    IntegerValueOfBox,
    /// `Long.valueOf(J)`: the same, round 12 wave 2 (C12-2).
    LongValueOfBox,
    /// `Object.getClass()`: two loads from the per-VM word table (nio20 w2).
    ObjectGetClass,
    /// The eight thin-bound `Unsafe` primitive-array accessors (nio20 w2).
    UnsafeAccess,
    /// `Integer.intValue()`: one compact/legacy load (nio20 w3, W2L2-2).
    IntegerIntValue,
    /// `Long.longValue()`: the same, 8 bytes (nio20 w3, W2L2-2).
    LongLongValue,
    /// `Thread.currentThread()`: the TLS thread word and one load (nio20 w4,
    /// lane L3; `DirectHelperTable::thread_java_obj_offset`).
    ThreadCurrentThread,
    /// The sixteen multi-byte `ScopedMemoryAccess` thin binds: a null-session
    /// heap or off-heap access, arena handles through the JIT-readable table
    /// (nio21 w3, lane L3, W2L3-3; [`sp_scoped_access_of`]).
    ScopedAccess,
    /// `System.identityHashCode(Object)`: inline header read or thin CAS mint.
    IdentityHashCode,
}

impl SpThinPrefix {
    /// Is this row's single-pass prefix switched on? The `valueOf` rows ask
    /// their own switches inside their emitters; the rest share
    /// [`sp_thin_prefix_enabled`].
    pub(in crate::x64::op_invoke) fn enabled(self) -> bool {
        match self {
            SpThinPrefix::IntegerValueOfBox | SpThinPrefix::LongValueOfBox => true,
            SpThinPrefix::IdentityHashCode => {
                sp_thin_prefix_enabled()
                    && sp_identity_hash_inline_enabled()
                    && crate::ir_lower::ir_identity_hash_inline_enabled()
                    && crate::identity_hash_direct_enabled()
            }
            SpThinPrefix::ObjectGetClass
            | SpThinPrefix::UnsafeAccess
            | SpThinPrefix::IntegerIntValue
            | SpThinPrefix::LongLongValue
            | SpThinPrefix::ThreadCurrentThread => sp_thin_prefix_enabled(),
            SpThinPrefix::ScopedAccess => {
                sp_thin_prefix_enabled() && sp_scoped_access_prefix_enabled()
            }
        }
    }
}

/// One row of [`SP_THIN_PREFIX_ROWS`].
pub(in crate::x64) struct SpThinPrefixRow {
    pub(in crate::x64) prefix: SpThinPrefix,
    /// The `DirectHelperTable` cell the site's `callee_entry` must equal.
    pub(in crate::x64) helper: fn(&crate::DirectHelperTable) -> usize,
    /// The instance arm (`true`, receiver first in the arguments) or the
    /// static arm.
    pub(in crate::x64) receiver: bool,
    /// The declared parameter count (receiver excluded); `None` when the
    /// prefix's emitter screens the shape itself (`Unsafe`).
    pub(in crate::x64) params: Option<usize>,
    /// The return type tag; `None` as above.
    pub(in crate::x64) ret: Option<u8>,
}

/// nio20 wave 3, lane L3 (W2L2-1): THE table of single-pass thin-helper
/// prefixes, consulted by both plain direct arms (`invokestatic` and the
/// instance `invokespecial`/`invokevirtual` arm) after the one admission
/// screen (`sp_thin_helper_prefix_admitted`). A new prefix is one row here
/// plus its emitter in `Compiler::try_emit_sp_thin_prefix`.
pub(in crate::x64) const SP_THIN_PREFIX_ROWS: &[SpThinPrefixRow] = &[
    SpThinPrefixRow {
        prefix: SpThinPrefix::IdentityHashCode,
        helper: |dh: &crate::DirectHelperTable| dh.identity_hash_code,
        receiver: false,
        params: Some(1),
        ret: Some(b'I'),
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::IntegerValueOfBox,
        helper: |dh: &crate::DirectHelperTable| dh.integer_value_of,
        receiver: false,
        params: Some(1),
        ret: Some(b'L'),
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::LongValueOfBox,
        helper: |dh: &crate::DirectHelperTable| dh.long_value_of,
        receiver: false,
        params: Some(1),
        ret: Some(b'L'),
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::ObjectGetClass,
        helper: |dh: &crate::DirectHelperTable| dh.object_get_class,
        receiver: true,
        params: Some(0),
        ret: Some(b'L'),
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::IntegerIntValue,
        helper: |dh: &crate::DirectHelperTable| dh.integer_int_value,
        receiver: true,
        params: Some(0),
        ret: Some(b'I'),
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::LongLongValue,
        helper: |dh: &crate::DirectHelperTable| dh.long_long_value,
        receiver: true,
        params: Some(0),
        ret: Some(b'J'),
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::ThreadCurrentThread,
        helper: |dh: &crate::DirectHelperTable| dh.thread_current_thread,
        receiver: false,
        params: Some(0),
        ret: Some(b'L'),
    },
    // nio21 wave 3, lane L4: `Thread.currentCarrierThread()`. Its registered
    // native is `currentThread`'s, so the helper's fast arm answers the same
    // mirror word and the same prefix serves it (published only where the
    // per-call gate can never refuse: `thread_java_obj_offset`).
    SpThinPrefixRow {
        prefix: SpThinPrefix::ThreadCurrentThread,
        helper: |dh: &crate::DirectHelperTable| dh.thread_current_carrier_thread,
        receiver: false,
        params: Some(0),
        ret: Some(b'L'),
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_get_byte,
        receiver: true,
        params: None,
        ret: None,
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_get_short,
        receiver: true,
        params: None,
        ret: None,
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_get_int,
        receiver: true,
        params: None,
        ret: None,
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_get_long,
        receiver: true,
        params: None,
        ret: None,
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_put_byte,
        receiver: true,
        params: None,
        ret: None,
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_put_short,
        receiver: true,
        params: None,
        ret: None,
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_put_int,
        receiver: true,
        params: None,
        ret: None,
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_put_long,
        receiver: true,
        params: None,
        ret: None,
    },
    // nio21 wave 4 L4: the volatile int/long thin binds; the prefix serves an
    // aligned primitive-array element and declines everything else (a field
    // offset, a null base) to the unchanged CALL.
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_get_int_volatile,
        receiver: true,
        params: None,
        ret: None,
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_get_long_volatile,
        receiver: true,
        params: None,
        ret: None,
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_put_int_volatile,
        receiver: true,
        params: None,
        ret: None,
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_put_long_volatile,
        receiver: true,
        params: None,
        ret: None,
    },
    // nio21 wave 5 L3: the `compareAndSet{Int,Long}` thin binds; the prefix
    // serves an aligned primitive-array element with one `LOCK CMPXCHG` and
    // declines everything else to the unchanged CALL.
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_cas_int,
        receiver: true,
        params: None,
        ret: None,
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_cas_long,
        receiver: true,
        params: None,
        ret: None,
    },
    // JIT round 20 wave 12, lane qUnsafe (AT-4): the `compareAndSetReference`
    // thin bind; the prefix serves a published compact reference field whose
    // barriers the published gates rule out, and declines everything else to
    // the unchanged CALL.
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        helper: |dh: &crate::DirectHelperTable| dh.unsafe_cas_reference,
        receiver: true,
        params: Some(4),
        ret: Some(b'Z'),
    },
    // JIT round 21 wave 1, lane updaters (UPDATERS21-1, opt-in): the
    // reference volatile get / put thin binds; the prefix serves a published
    // compact reference field (a put only where the published gates rule the
    // barriers out) and declines everything else to the unchanged CALL.
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        // Unwired (`0`, no row) with the switch off: the default site is untouched.
        helper: |dh: &crate::DirectHelperTable| {
            if crate::direct_helpers::unsafe_ref_volatile_field_enabled() {
                dh.unsafe_get_reference_volatile
            } else {
                0
            }
        },
        receiver: true,
        params: Some(2),
        ret: Some(b'L'),
    },
    SpThinPrefixRow {
        prefix: SpThinPrefix::UnsafeAccess,
        // Unwired (`0`, no row) with the switch off: the default site is untouched.
        helper: |dh: &crate::DirectHelperTable| {
            if crate::direct_helpers::unsafe_ref_volatile_field_enabled() {
                dh.unsafe_put_reference_volatile
            } else {
                0
            }
        },
        receiver: true,
        params: Some(3),
        ret: Some(b'V'),
    },
];

/// The [`SP_THIN_PREFIX_ROWS`] row a plain direct site matches: its bound
/// `callee_entry` is the row's helper cell (never an unwired `0`), at the
/// row's arm, with the row's declared shape. `None`: no prefix, the call
/// stands alone.
pub(in crate::x64) fn sp_thin_prefix_row(
    dh: &crate::DirectHelperTable,
    callee_entry: usize,
    receiver: bool,
    callee_params: usize,
    ret_type: u8,
) -> Option<SpThinPrefix> {
    if callee_entry == 0 {
        return None;
    }
    SP_THIN_PREFIX_ROWS
        .iter()
        .find(|row| {
            row.receiver == receiver
                && (row.helper)(dh) == callee_entry
                && row.params.is_none_or(|p| p == callee_params)
                && row.ret.is_none_or(|t| t == ret_type)
        })
        .map(|row| row.prefix)
        // nio21 wave 3 (lane L3): the sixteen `ScopedMemoryAccess` cells are
        // one family with its own shape table (`sp_scoped_access_of`), so
        // they are matched there rather than as sixteen rows. Instance arm.
        .or_else(|| {
            sp_scoped_access_of(dh, callee_entry)
                .filter(|a| receiver && a.num_params() == callee_params && a.ret_type() == ret_type)
                .map(|_| SpThinPrefix::ScopedAccess)
        })
}

/// Round 13 (lane crash): the inline MIC/PIC native-stack guard
/// (`CRATONVM_JIT_IC_STACK_GUARD`, default on; `0` emits the IC hit path
/// exactly as before). Read per site compile, so it adds no process global.
pub(in crate::x64::op_invoke) fn ic_stack_guard_enabled() -> bool {
    true
}



/// gcd d9/d (2026-09-28): `CRATONVM_JIT_CALLEE_OWNED_ARGS` (default on; `0`
/// keeps the service copy of a baked direct call's arguments exactly as
/// before). Read per site compile, so it adds no process global and a test's
/// thread override reaches it.
pub(in crate::x64::op_invoke) fn callee_owned_args_enabled() -> bool {
    true
}

/// gcd d9/d (2026-09-28): may this baked single-pass direct `CALL` leave its
/// arguments to the callee? The SITE half of the rule; the callee half is
/// `crate::direct_callee_owns_its_arguments`, asked after this answers `true`.
///
/// Every term keeps a reader of the arguments that runs after the call:
///
/// * `opcode == invokestatic` and `invoke_kind == 3`, or (gcd d10/f,
///   2026-09-28, `walk_invoke_instance`'s plain direct arm) an instance opcode
///   whose site is statically bound, `invoke_kind == 1` (every `invokespecial`,
///   and a private `invokevirtual` / `invokeinterface` the single-pass site
///   planner pins to its owner): the service resolves such a callee by name
///   (through the pinned owner id), and reads no receiver out of the arguments
///   (`jit_service_callee_deopt_body`'s `receiver_class_id` is asked only for
///   kinds 0/2, whose receiver class selects the method; those keep the copy);
/// * not a caller-held synchronized site (either row): its exits read the
///   receiver back from the service copy;
/// * not a retire-cell site: the cell's dispatch fallback takes the service
///   copy as its argument buffer;
/// * not a protected pc: with the copy gone, a trap the service can no longer
///   re-run leaves this frame as a frameless deopt through the shared
///   sentinel exit, and only an unprotected invoke's exception check takes
///   that exit (a protected one goes to a local-handler stub or a reason-9
///   frame, which expect a pending exception);
/// * `caller_replay_exact` (gcd d10/f, 2026-09-28; asked last, it scans the
///   bytecode): that frameless deopt is
///   answered by the caller's caller RE-RUNNING THIS METHOD FROM ENTRY, so it
///   is admitted only where that re-run commits nothing twice
///   (`Compiler::owned_args_handoff_replays_exactly`). Without it a rare
///   callee decline (a redefinition mid-call, a rematerialisation that fails
///   after a collection) replayed every store, invoke and monitor operation
///   this method made before the call
///   (`gcd-d10f-owned-args-handoff-replays-the-caller-unchecked-DONE-20260928.md`).
///   A refused site keeps the copy, and its service re-runs only the callee,
///   as before d9/d.
///
/// Pure, so the table is testable without a compile.
pub(in crate::x64::op_invoke) fn sp_direct_call_args_owned_by_callee(
    enabled: bool,
    opcode: Option<u8>,
    invoke_kind: Option<u8>,
    sync_direct_site: bool,
    retire_cell: bool,
    pc_is_protected: bool,
    caller_replay_exact: impl FnOnce() -> bool,
) -> bool {
    let statically_bound = matches!(
        (opcode, invoke_kind),
        (Some(0xb8), Some(3)) | (Some(0xb6 | 0xb7 | 0xb9), Some(1))
    );
    enabled
        && statically_bound
        && !sync_direct_site
        && !retire_cell
        && !pc_is_protected
        && caller_replay_exact()
}



/// gcd d9/d row 4 (the gcj j2/pn2 residual, `Gcd1ArgPinSpecialProbe`
/// `spliced-direct`): may a single-pass splice's direct call leave its
/// arguments to its callee? [`sp_direct_call_args_owned_by_callee`] with the
/// splice's terms: no spliced call is a caller-held synchronized site or a
/// retire cell, `pc_is_protected` is asked of the splice's own call site, and
/// `replay_exact` holds the rest (both replay levels, the callee's ownership;
/// `Compiler::splice_inner_call_owned_args`). Asked last, as it scans
/// bytecode.
pub(in crate::x64) fn splice_call_args_owned_by_callee(
    opcode: Option<u8>,
    invoke_kind: Option<u8>,
    pc_is_protected: bool,
    replay_exact: impl FnOnce() -> bool,
) -> bool {
    sp_direct_call_args_owned_by_callee(
        callee_owned_args_enabled(),
        opcode,
        invoke_kind,
        false,
        false,
        pc_is_protected,
        replay_exact,
    )
}

/// j27 wave 9 (lane W9RPL; OOM21-2): [`caller_replay_to_call_is_exact`]
/// plus the opt-in constant-`putstatic` arm
/// (`deopt::owned_args_caller_replay_is_exact_with_statics`), with the
/// single-pass tier's resolved static rows (`Compiler::static_field_info`)
/// vouching for each `putstatic` ([`sp_static_row_is_plain`]). The IR tier
/// vouches the same way from its `Op::StoreStatic` nodes; the service-copy
/// window already counts both (`deopt::owned_args_caller_replay_may_be_exact`).
pub(in crate::x64::op_invoke) fn caller_replay_to_call_is_exact_with_statics(
    code: &[u8],
    code_len: usize,
    pc: usize,
    handlers: &[(usize, usize, usize)],
    static_rows: &[(usize, u32, usize, u8, bool)],
) -> bool {
    u32::try_from(pc).is_ok_and(|bci| {
        crate::deopt::owned_args_caller_replay_is_exact_with_statics(
            code,
            code_len,
            bci,
            handlers,
            &|at: usize| sp_static_row_is_plain(static_rows, at),
        )
    })
}

/// Does the single-pass compile's static row at `pc` name a resolved
/// (`class_id != 0`), non-volatile static? Rows are
/// `(pc, class_id_raw, field_index, type_tag, is_volatile)`.
pub(in crate::x64::op_invoke) fn sp_static_row_is_plain(static_rows: &[(usize, u32, usize, u8, bool)], pc: usize) -> bool {
    static_rows
        .iter()
        .any(|&(at, class_id, _, _, is_volatile)| at == pc && class_id != 0 && !is_volatile)
}

impl Compiler {
    /// `caller_replay_to_call_is_exact` for this compile: `false` on
    /// rewritten bytecode (`bci_provenance`) and in a self-locking body
    /// (`self_lock_obj_off`), both set before the walk. j27 wave 9: with the
    /// constant-`putstatic` arm ([`caller_replay_to_call_is_exact_with_statics`]).
    pub(in crate::x64) fn owned_args_handoff_replays_exactly(
        &self,
        code: &[u8],
        code_len: usize,
        pc: usize,
    ) -> bool {
        self.bci_provenance.is_none()
            && self.self_lock_obj_off == 0
            && caller_replay_to_call_is_exact_with_statics(
                code,
                code_len,
                pc,
                &self.replay_handler_ranges,
                &self.static_field_info,
            )
    }
}



/// gcd d9/d row 2 (the gcj j2/pn2 residual, `Gcd1ArgPinSpecialProbe`
/// `virtual-direct` / `interface-direct`): which words of an inline-cache
/// site's args buffer a MIC/PIC hit files as its service copy for the
/// collector's window (`CompiledMethod::service_arg_copies`, gkr OOM21-2):
/// every reference argument but the receiver, by `arg_oops`' index.
///
/// A monomorphic `invokevirtual` / `invokeinterface` lowers to the inline
/// cache, whose one buffer feeds the hit's marshal, the `.miss` helper and,
/// after a hit returns the sentinel, the site-open service. Named in the
/// call's map, it kept every argument for the whole call however early the
/// callee dropped it, where HotSpot's argument is the callee's. Filed at the
/// hit's return address, those words are cleared by the collector once the
/// callee that ran stands past its last exact re-run point
/// (`conservative_roots::clear_closed_service_copies`, which asks the callee
/// frame itself, so whatever the cache bound is judged): from there the
/// service never reads them. The receiver stays: the service reads its class
/// to select the callee (`jit_service_callee_deopt_body`). The miss helper,
/// the hashed stub and the vtable column return elsewhere, so their buffer is
/// whole. All `false` with the switch off.
pub(in crate::x64::op_invoke) fn ic_hit_window_arg_oops(arg_oops: &[bool]) -> Vec<bool> {
    let on = true;
    arg_oops
        .iter()
        .enumerate()
        .map(|(i, &oop)| on && oop && i > 0)
        .collect()
}

/// The outgoing stack block for a single-pass MIC hit whose `n` arguments
/// (receiver included) do not fit `regs` argument registers together with the
/// context word: `Some((total_sub, stack_base))`, or `None` when nothing
/// overflows or more than `crate::inline_cache_pic::ic_max_stack_words()`
/// would (4 until round 13 wave 11, 8 since: lane mega9, proposal M13-8,
/// `CRATONVM_JIT_IC_WIDE_STACK_WORDS`). A bound on code size at a site, not
/// an ABI limit: every compiled prologue reads any number of caller stack
/// words.
///
/// Sized for the CONTEXT ABI (`vm_ptr` + `n` words), which always needs one
/// more stack word than the context-free ABI, so one `SUB`/`ADD` pair of the
/// same size serves both paths of the hit; the context-free path leaves its
/// top word unused. The layout is `emit_stack_arg_setup`'s: Win64 reserves the
/// 32-byte home area below the stack words (`stack_base == 32`), SysV none,
/// and the total is rounded up to 16 so RSP stays aligned at the CALL.
pub(in crate::x64::op_invoke) fn ic_stack_arg_block(n: usize, regs: usize, win64: bool) -> Option<(i32, i32)> {
    let words = (n + 1).checked_sub(regs).filter(|&w| w > 0)?;
    if words > crate::inline_cache_pic::ic_max_stack_words() {
        return None;
    }
    let shadow: i32 = if win64 { 32 } else { 0 };
    let raw = shadow + i32::try_from(words).ok()? * 8;
    Some(((raw + 15) & !15, shadow))
}

// ---------------------------------------------------------------------------
// Post-call exit sites (interpreter round i1 wave 27, lane L6)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/profile-guided-inlining.md`,
// stages 1-2, for the single-pass tier's OSR compiles. After a real call whose
// successor state an `OsrExit` map can describe, the walk emits a five-byte
// `NOP` (`crate::ENTRY_PATCH_PAD`) and files the map at the SUCCESSOR bci; after
// the walk each site gets a 16-byte stub (`CALL` the shared verdict tail, `JNZ`
// the successor's reason-7 stub, `JMP` back past the `NOP`). A class
// redefinition that withdraws the body rewrites the `NOP` into a `JMP` to the
// stub (`JitCache::force_withdrawn_exit_polls`, with the body's exit polls), so
// a frame whose thread was inside the call leaves for the interpreter as soon
// as the call returns, before the rest of its iteration runs an old splice --
// what HotSpot's return-address patch to the deopt blob does. Until then the
// fast path is the `NOP` (and, at most one site in two, a one-to-four-byte
// alignment `NOP` in front of it).

/// Kill switch for the post-call exit sites. `false` emits no site, no stub
/// and no successor map: every compile is byte-identical to wave 26's.
pub(in crate::x64) const POST_CALL_EXITS_ENABLED: bool = true;

/// Kill switch for post-call exit sites in a METHOD-ENTRY single-pass compile
/// (interpreter round i1 wave 29, lane L3;
/// `interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007`,
/// the "next single-pass step" of the proposal's "Why OSR compiles only"). Such
/// a body gets a site only once it already carries a loop-boundary exit map
/// (`Compiler::osr_exit_points` non-empty when the walk reaches the call: a
/// loop header before it, or an `invokedynamic` trap map), so no body that had
/// none gains its first deopt exit here -- the caller-held direct `CALL` route
/// for synchronized callees (`jit_bridge::wrapped_body_has_no_deopt_exit`)
/// answers as before. The exit leaves on the method-entry verdict bit
/// (`SAFEPOINT_VERDICT_POLLING_BODY`), as the body's own back-edge polls do.
/// `false` restores wave 28's OSR-only rule.
pub(in crate::x64) const METHOD_ENTRY_POST_CALL_EXITS_ENABLED: bool = true;

/// Kill switch for a METHOD-ENTRY single-pass post-call site that files the
/// body's FIRST loop-boundary exit map: a call before the first loop header
/// in bytecode order, or in a loop-free body (interpreter round i1 wave 39,
/// lane L3; `interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007`,
/// "What remains"). Admitted only when the compile keeps a dispatch record
/// (`Compiler::keeps_a_call_record`): such a body calls something by its
/// artifact's own record (`_jit_invoke_infos`, `_jit_mic_slots`,
/// `_jit_pic_slots`), so the two routes wave 29 kept the first deopt exit
/// away from -- the caller-held direct `CALL` of a synchronized callee
/// (`jit_bridge::caller_held_body_is_closed`) and the synchronized door's
/// wrapped body (`door_admits_wrapped_body`) -- already refuse it through
/// `wrapped_body_attempt_never_reruns`'s "calls nothing" test, whatever its
/// deopt exits. The other reader the first map changes, `can_osr_exit`, is
/// read by the OSR door's transfer only, which never enters a method-entry
/// body. The wave-38 argument for the optimizing tier, applied here.
/// `false` restores wave 29's rule (a map filed before the call).
pub(in crate::x64) const METHOD_ENTRY_FIRST_MAP_POST_CALL_EXITS_ENABLED: bool = true;

/// Switch for a METHOD-ENTRY single-pass post-call site that files the body's
/// FIRST loop-boundary exit map in a compile that keeps NO dispatch record,
/// when the door said the method is not `ACC_SYNCHRONIZED`
/// (`BackendRequest::method_known_unsynchronized`; interpreter round i2 wave
/// 54, lane L2: item 1 of
/// `interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007`).
///
/// The dispatch-record test of [`METHOD_ENTRY_FIRST_MAP_POST_CALL_EXITS_ENABLED`]
/// exists only to keep the first deopt exit off a body that the two
/// synchronized routes must see exit-free: the caller-held direct `CALL`
/// (`jit_bridge::caller_held_body_is_closed`) and the synchronized door's
/// wrapped body (`door_admits_wrapped_body`), both asked only about an
/// `ACC_SYNCHRONIZED` method's body. The other readers of a body's first
/// exit map are the OSR door's (`can_osr_exit`, `cached_osr_body_is_despec_stale`:
/// OSR bodies only) and the door's stash read (`body_may_publish_a_pad_frame`:
/// one more length load per such call), so for an unsynchronized method the
/// site is admitted whatever its calls go through: a body whose only calls
/// are intrinsic helpers that can reach a safepoint now leaves after such a
/// call when a redefinition forces it, instead of finishing its activation
/// on a splice of the redefined class. `false` keeps the wave-39 rule. `true`
/// since the wave-54 lane's last commit.
pub(in crate::x64) const METHOD_ENTRY_UNSYNCHRONIZED_FIRST_MAP_POST_CALL_EXITS: bool = true;

/// Kill switch for the constant-pool helper exit sites (interpreter round i1
/// wave 44, lane L2;
/// `interpreter-L2-proposal-exits-at-a-withdrawn-bodys-constant-pool-helper-calls-RETIRED-20261015`):
/// a patchable site BEFORE each run-time constant-pool helper call (`ldc`
/// String / Class, a deferred `new` / `anewarray`, `multianewarray`), with an
/// `OsrExit` map at the instruction ITSELF, so a body a redefinition or a
/// debugger withdrew leaves for the interpreter at the instruction instead of
/// running it compiled (and translating its constant-pool index). The same
/// site, stub, verdict tail, cap and force pass as the post-call exits
/// ([`Compiler::emit_cp_helper_exit_site`]). `false` emits no such site, and
/// every compile is byte-identical to wave 43's.
pub(in crate::x64) const CP_HELPER_EXITS_ENABLED: bool = true;

// Post-call exit CLAIMS (interpreter round i1 wave 48, lane L2;
// `docs/feature-designs/jit-ir-optimizations.md`):
// every single-pass post-call site builds its map at the call's return but
// records only a CLAIM ([`PostCallExitClaim`]); after the walk
// ([`Compiler::resolve_post_call_exit_claims`]) the claim
//
// * files its map, when nothing else filed a non-rethrow point at the
//   successor's bci;
// * shares the one `OsrExit` map filed there, when it describes the same
//   frame (the stub's `JNZ` resolves by bci, so the site and the successor's
//   own exit leave through one point);
// * is dropped otherwise (the `NOP` stays, never recorded, never forced):
//   the refusal is decided by what was filed, not by a prediction.
//
// So the successor admission covers every instruction that can follow a call
// in the same basic block ([`post_call_exit_claim_successor_admitted`]). The
// wave-47 admission that filed the map at once, and so had to PREDICT every
// successor whose own lowering files a point at that bci (the wave-46
// successor list with its `getstatic` / `getfield` switch, and the
// rotated-entry `goto` and fused back-edge constant refusals), sat behind the
// `false` arm of the `POST_CALL_EXIT_CLAIMS_ENABLED` const from wave 48 and
// was deleted in interpreter round i2 wave 62 (lane L2;
// `docs/feature-designs/jit-ir-optimizations.md`):
// those two shapes are now decided by what is filed (the pre-header guards
// are a point of another reason, the fused back edge's map one the claim
// shares or yields to).

/// At most this many post-call exit sites per compile. Each costs a successor
/// map, a stub of 16 bytes and a reason-7 stub; past the cap a call keeps
/// wave 26's behaviour (its frame leaves at the next exit-capable back edge).
/// The constant-pool helper exit sites (wave 44) count against it too, and so
/// do unresolved claims ([`PostCallExitClaim`]). 64 until
/// interpreter round i1 wave 48; 1024 under [`POST_CALL_EXIT_SITES_WIDE_CAP`]
/// until 2026-10-07; since then [`POST_CALL_EXIT_SITES_UNREACHABLE_CAP`]
/// under [`POST_CALL_EXIT_SITES_UNCAPPED`] (item 4 of
/// `interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007`).
pub(in crate::x64) const POST_CALL_EXIT_SITES_MAX: usize = if POST_CALL_EXIT_SITES_UNCAPPED {
    POST_CALL_EXIT_SITES_UNREACHABLE_CAP
} else if POST_CALL_EXIT_SITES_WIDE_CAP {
    1024
} else {
    64
};

/// A cap no method reaches: a method's code is at most 65535 bytes
/// (JVMS 4.7.3), and every site follows an instruction of at least one byte
/// (a call, or a constant-pool helper instruction of at least two), so a
/// compile can never queue this many. Kept as a number rather than removed
/// so that the per-compile counters stay bounded by construction.
pub(in crate::x64) const POST_CALL_EXIT_SITES_UNREACHABLE_CAP: usize = 65_536;

/// Kill switch for lifting the post-call site cap (item 4 of the i24-L6
/// page: "a call past the 1024th site of a single-pass compile"; 2026-10-07).
/// The cap bounded only a pathological method's metadata, and that
/// metadata grows linearly with the calls without it, as the first 1024
/// sites' already did: one frame-wide point per call, beside the oop map the
/// call already keeps (and, inside a try range, the `RETHROW` frame of the
/// same width that `emit_post_invoke_exception_check` files for it). The
/// sites are `NOP`s until a redefinition forces them, so the cost is
/// metadata only. Past the 1024th site such a method (only one of more than
/// 3000 bytes of calls: HotSpot does not compile it at all,
/// `DontCompileHugeMethods`, so its frame returns into the interpreter at
/// every call's return there) ran the rest of the iteration after a call
/// whose splice a redefinition withdrew on the old bytecode once. `false`
/// restores the 1024 of [`POST_CALL_EXIT_SITES_WIDE_CAP`].
pub(in crate::x64) const POST_CALL_EXIT_SITES_UNCAPPED: bool = true;

/// Kill switch for the `invokedynamic`, `monitorenter` and `monitorexit`
/// successors of a post-call exit claim
/// ([`post_call_exit_claim_successor_admitted`]; item 2 of the i24-L6 page,
/// 2026-10-07). `false` refuses them, as from wave 48.
pub(in crate::x64) const POST_CALL_EXIT_CLAIMS_AT_INDY_AND_MONITORS: bool = true;

/// Kill switch for the post-call exit sites after a `getstatic` /
/// `putstatic` helper call that may run `<clinit>`
/// ([`Compiler::emit_post_clinit_helper_exit_site`]; the i24-L6 page's
/// "Fixed (2026-10-07)"). `false` emits none, as before.
pub(in crate::x64) const POST_CLINIT_HELPER_EXIT_SITES: bool = true;

/// Kill switch for a LOCAL exception handler's entry as the successor of a
/// post-call exit claim, reached live by the call's fall-through
/// ([`forward_only_joins`]; item 2 of the i24-L6 page, 2026-10-07). `false`
/// refuses every handler entry, as from wave 60.
pub(in crate::x64) const POST_CALL_EXIT_CLAIMS_AT_HANDLER_ENTRIES: bool = true;

/// Kill switch for the wider post-call exit site cap (interpreter round i1
/// wave 48, lane L2; item 4 of "Progress (wave 40)" of
/// `interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007`:
/// "a call past a compile's 64th site"). The 64 was never a limit of the
/// mechanism: a site is a five-byte `NOP` (plus at most a four-byte pad), a
/// 16-byte stub and one `OsrExit` point, each proportional to the call it
/// follows, which already carries its own oop map and exception frame; the
/// force pass (`not_entrant::exit_poll_writes`) iterates a `Vec`. The cap is
/// kept only as a bound on a pathological method's metadata. The optimizing
/// tier's `IR_POST_CALL_EXIT_SITES_MAX` is 512 since interpreter round i2
/// wave 54 (its graphs admit 64 own calls, plus the real calls of its spliced
/// bodies, which have post-call sites since wave 47). `false` restores the 64.
pub(in crate::x64) const POST_CALL_EXIT_SITES_WIDE_CAP: bool = true;

/// A post-call exit site whose map is not filed yet (a CLAIM, interpreter
/// round i1 wave 48): the patchable `NOP` at `site`, the
/// call's successor at emitter pc `bci`, and the `OsrExit` point built at the
/// call's return (`Compiler::build_deopt_point`: the frame as the `NOP` sees
/// it, stamped with the `NOP`'s buffer position).
pub(crate) struct PostCallExitClaim {
    pub(in crate::x64::op_invoke) site: usize,
    pub(in crate::x64::op_invoke) bci: usize,
    pub(in crate::x64::op_invoke) point: crate::deopt::DeoptimizationPoint,
    /// `bci` is a loop header ([`post_call_exits_at_loop_headers_enabled`]):
    /// the claim may only share the header's map, never file its own.
    pub(in crate::x64::op_invoke) loop_header: bool,
}

/// How [`Compiler::resolve_post_call_exit_claims`] settled one claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::x64::op_invoke) enum PostCallExitClaimOutcome {
    /// Nothing else filed a non-rethrow point at the bci: the claim's own map.
    /// Since wave 49 ([`crate::osr_exit::OSR_EXIT_MAPS_BESIDE_GUARDS`]) also
    /// when everything else there is a `REEXECUTE` guard of the claim's frame
    /// ([`guard_admits_an_exit_map_beside`]).
    Own,
    /// The one non-rethrow point there is an `OsrExit` map of the same frame.
    Shared,
    /// Only `REEXECUTE` guards of ANOTHER frame are there (the next invoke's
    /// dispatch-helper snapshot, taken after its arm flushed a `float` /
    /// `double` result the claim's map saw in its XMM register), no
    /// `OsrExit` map is keyed at the bci,
    /// and the compile is a METHOD-ENTRY one (since wave 56 also an OSR-tier
    /// one, [`osr_post_call_own_point_exits_enabled`]): the claim's point is
    /// recorded in `deopt_points` but not filed by bci, and its stub bakes it
    /// itself (tag 13, [`post_call_own_point_exits_enabled`]). Interpreter
    /// round i2 wave 55, lane L2.
    OwnBeside,
    /// Another point is there: the site is left an unrecorded `NOP`.
    Dropped,
}

/// `CRATONVM_JIT_POST_CALL_OWN_POINT_EXITS` -- interpreter round i2 wave 55,
/// lane L2: build
/// `docs/feature-designs/jit-ir-optimizations.md`
/// (item 2 of
/// `interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007.md`).
///
/// On: a single-pass post-call exit claim whose successor filed only
/// `REEXECUTE` guards of a DIFFERENT frame at its bci (measured: `g(f())`
/// where `f` returns a `float` or `double`, which the claim's map reads from
/// its XMM register and `g`'s dispatch-helper snapshot from its frame word;
/// an `int`, `long` or reference result is in its frame word at the return,
/// so that claim is wave 49's `Own`) is settled
/// [`PostCallExitClaimOutcome::OwnBeside`] instead of `Dropped`, in a
/// METHOD-ENTRY compile only: the point goes into `deopt_points` (so every
/// stash sink's `owns_deopt_point` and the cause check know it), NOT into
/// `osr_exit_box_ptr_by_bci` / `osr_exit_points` (OSR entry, the OSR door's
/// exit classification and the loop-boundary readers never see it), and its
/// stub (tag 13 of `emit_deopt_stubs`) bakes it from
/// `Compiler::post_call_own_box_ptr_by_bci`. Every cause-less by-bci reason
/// lookup already skips an `OsrExit` point beside a guard
/// (`osr_exit::trap_reason_at_bci`, wave 49), and the stub stashes the
/// point's own cause. Off (`=0`): such a claim is `Dropped`, as in waves
/// 48-54. **Default ON** since the wave-55 lane's last commit (revert that
/// commit to turn it off by default).
///
/// Read once per compile that settles a claim; not cached in a static
/// (`process_global_statics_ratchet`).
pub(crate) fn post_call_own_point_exits_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_OSR_POST_CALL_OWN_POINT_EXITS` -- interpreter round i2 wave
/// 56, lane L2: the OSR-tier stage of
/// `docs/feature-designs/jit-ir-optimizations.md`
/// (item 2 of
/// `interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007.md`).
///
/// On (with [`post_call_own_point_exits_enabled`]): an OSR-tier compile
/// (`mode_exit_polls`) settles a claim [`PostCallExitClaimOutcome::OwnBeside`]
/// as a method-entry one does, when the compile already filed a loop-boundary
/// or back-edge exit map (so the artifact is `can_osr_exit` and the OSR door's
/// transfer takes the exit). The OSR door's by-bci readers know such a point
/// through `CompiledMethod::post_call_own_exit_bcis`: the exit classification
/// answers `PostCallOwnPoint` instead of the `ExitMapMissing` cross-check, and
/// the OSR entry contract (`osr_entry_frame_state`) never takes it for the
/// guard's frame at that bci. `resume_after_exit` needs nothing: the claim's
/// point and the guards beside it agree on `resume_kind`
/// (`Compiler::claim_may_bake_its_own_point`), so `resume_image` stays
/// unique there. Off (`=0`): such a claim is `Dropped` in an OSR-tier
/// compile, as in waves 48-55. **Default ON** since the wave-56 lane's
/// switch-on commit (revert that commit to turn it off by default).
///
/// Read once per OSR-tier compile that settles a claim; not cached in a
/// static (`process_global_statics_ratchet`).
pub(crate) fn osr_post_call_own_point_exits_enabled() -> bool {
    true
}

/// The `emit_deopt_stubs` tag of a post-call exit stub that bakes its own
/// point ([`PostCallExitClaimOutcome::OwnBeside`]); 7 is the tag of one that
/// resolves through `osr_exit_box_ptr_by_bci`.
pub(in crate::x64) const POST_CALL_OWN_POINT_STUB_TAG: i64 = 13;

/// The `emit_deopt_stubs` tag of a post-call exit stub after a real call
/// inside a splice ([`Compiler::emit_splice_post_call_exit_site`]): its
/// `deopt_stubs` entry's `site_pc` is an index into
/// `Compiler::splice_post_call_box_ptrs`, not a bci. Interpreter round i2
/// wave 59, lane L2.
pub(in crate::x64) const SPLICE_POST_CALL_STUB_TAG: i64 = 14;

/// The `deopt_point_pcs` entry of a splice post-call exit's point: no pc of
/// the compile's own method (see [`Compiler::emit_splice_post_call_exit_site`]).
pub(in crate::x64) const SPLICE_POINT_EMITTER_PC: usize = usize::MAX;

/// `CRATONVM_JIT_SP_POST_CALL_SPLICE_EXITS` -- interpreter round i2 wave 59,
/// lane L2: the single-pass stage of
/// `docs/feature-designs/deopt-osr.md`.
/// On: a single-pass METHOD-ENTRY compile puts a post-call exit site after a
/// real call made inside an outermost splice, leaving through a two-frame
/// chain ([`Compiler::emit_splice_post_call_exit_site`]), and the splice
/// flushes its scratch registers before it captures the caller scope. Off:
/// no such site, byte for byte as before (a frame inside such a call
/// finishes the splice on the old bytecode and leaves at its next
/// exit-capable site). **Default ON** since the wave-59 lane's switch-on
/// commit (revert that commit to turn it off by default). Read per spliced
/// call of a compile; not cached in a static (`process_global_statics_ratchet`).
pub(crate) fn sp_post_call_splice_exits_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_OSR_POST_CALL_SPLICE_EXITS` -- interpreter round i2 wave
/// 62, lane L2: the OSR-tier stage of the single-pass splice post-call exits
/// (the "call inside a splice of a single-pass OSR-tier compile" member of
/// `docs/feature-designs/jit-ir-optimizations.md`).
///
/// On (with [`sp_post_call_splice_exits_enabled`]): a single-pass OSR-tier
/// compile (`mode_exit_polls`) that already filed a loop-boundary exit map
/// (so the artifact is `can_osr_exit` and the OSR door transfers its exits)
/// puts the wave-59 post-call exit site after a real call made inside an
/// outermost splice. Its two-frame chain point leaves through the OSR door's
/// planned chain transfer (`deopt_resume::transfer_osr_exit_chain_into_live_frame`,
/// `RecordedBci`: the looping method's live frame is rewritten to park in its
/// `invoke` of the spliced callee, and the callee is pushed at the call's
/// successor with the result on its stack) -- the transfer every chain guard
/// of a single-pass OSR splice already takes. The point is recorded, never
/// filed by bci, and the entry contract skips every chain point
/// (`CompiledMethod::osr_entry_frame_state`), so the header's entry map is
/// untouched. Off: an OSR-tier compile emits no such site, byte for byte as
/// before (a frame inside such a call finishes the splice on the old
/// bytecode and leaves at the loop's next exit-capable back edge). **Default
/// ON** since the wave-62 lane's switch-on commit (revert that commit to turn
/// it off by default; `=0` turns it off). Read per spliced call of an
/// OSR-tier compile; not cached in a static (`process_global_statics_ratchet`).
pub(crate) fn sp_osr_post_call_splice_exits_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_SP_NESTED_POST_CALL_SPLICE_EXITS` -- the last call shape of
/// `docs/feature-designs/jit-ir-optimizations.md`
/// with no exit in a single-pass body: a real call inside a NESTED splice
/// (`docs/feature-designs/deopt-osr.md`).
///
/// On (with [`sp_post_call_splice_exits_enabled`]): a nested splice records
/// the enclosing callee parked in its `invoke` ([`NestedSpliceLevel`]), and
/// the wave-59 site after a real call inside the nested body files a chain
/// point of one frame per spliced level -- the innermost callee at the
/// call's successor with the result pushed, each enclosing callee parked in
/// its `invoke`, the compile's own method parked in its `invoke` -- which
/// the nested splice's postcondition admits (it names exactly the points the
/// sites queued). Off: no level is recorded and every site inside a nested
/// splice is refused, byte for byte as before. **Default ON.** Read once per
/// nested splice and per site; not cached in a static
/// (`process_global_statics_ratchet`).
pub(crate) fn sp_nested_post_call_splice_exits_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_POST_CALL_EXITS_AT_FORWARD_JOINS` -- interpreter round i2
/// wave 60, lane L2 (item 2 of
/// `docs/feature-designs/deopt-osr.md`).
/// On: a single-pass post-call exit CLAIM may name a successor that is a
/// branch target when it is a forward-only join ([`forward_only_joins`]) --
/// the shape of a call that ends an `if` arm with no `else`, whose successor
/// is the join, so a frame inside that call when its body was withdrawn ran
/// the rest of the iteration on the old splice. Off: every branch-target
/// successor is refused, byte for byte as before. **Default ON** since the
/// wave-60 lane's switch-on commit (revert that commit to turn it off by
/// default). Read once per single-pass walk; not cached in a static
/// (`process_global_statics_ratchet`).
pub(crate) fn post_call_exits_at_forward_joins_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_POST_CALL_EXITS_AT_LOOP_HEADERS` -- interpreter round i2
/// wave 61, lane L2
/// (`docs/feature-designs/jit-ir-optimizations.md`).
/// On: a single-pass post-call exit claim may name a successor that is a LOOP
/// HEADER (a textual back edge's target: `x = prepare(); while (...)`), but
/// only to SHARE the header's own `OsrExit` map
/// ([`PostCallExitClaimOutcome::Shared`]: one non-rethrow point there, the
/// keyed `OsrExit` map, describing the claim's frame value by value and
/// location by location). The site's stub then resolves to the header's map,
/// which OSR entry and the header's back-edge polls already read by bci; the
/// claim never files (`Own`) or bakes (`OwnBeside`) a map of its own there,
/// so the header's entry contract is untouched. Every other outcome drops
/// the claim. Off: every loop-header successor is refused, byte for byte as
/// before. **Default ON** since the wave-61 lane's switch-on commit (revert
/// that commit to turn it off by default). Read once per single-pass walk
/// (compile time); not cached in a static.
pub(crate) fn post_call_exits_at_loop_headers_enabled() -> bool {
    true
}

/// `targets[pc]`: is `pc` the target of a textual back edge of `code` (a loop
/// header, by the rule [`forward_only_joins`] refuses)? Built once per walk
/// ([`Compiler::post_call_loop_headers`]).
pub(in crate::x64) fn back_edge_targets(code: &[u8], code_len: usize) -> Vec<bool> {
    let mut targets = vec![false; code_len + 1];
    for (target, _source) in crate::bytecode_analysis::back_edges(code, code_len) {
        if let Some(t) = targets.get_mut(target) {
            *t = true;
        }
    }
    targets
}

/// [`back_edge_targets`] for one pc: the debug census's question
/// (`Compiler::note_post_call_header_claim_census`), asked per refused claim.
pub(in crate::x64::op_invoke) fn is_back_edge_target(code: &[u8], code_len: usize, pc: usize) -> bool {
    crate::bytecode_analysis::back_edges(code, code_len)
        .into_iter()
        .any(|(target, _)| target == pc)
}

/// `joins[pc]`: may a post-call exit claim name `pc` although it is a branch
/// target ([`Compiler::post_call_forward_joins`])? Only when every edge into
/// it is an explicit FORWARD branch or switch entry and the fall-through:
/// not pc 0 (the method entry), not an exception handler's entry (an implicit
/// edge), not the target of any textual back edge
/// (`bytecode_analysis::back_edges`: a loop header, whose map OSR entry and
/// the back-edge polls read by bci), and not a loop-boundary map's bci
/// (`osr_exit_map_headers`, which also names a rotated loop's dominating
/// test). The site's `NOP` follows the call on the fall-through edge alone,
/// before the walk binds the join's label and canonicalises its operands, so
/// its map describes exactly the frame that edge brings; the other edges never
/// pass it. Nothing else reads an `OsrExit` map at such a bci: a forward
/// branch polls nowhere, and the claim's resolution still yields to any point
/// another arm files there.
///
/// A LOCAL exception handler's entry is a join too under
/// [`POST_CALL_EXIT_CLAIMS_AT_HANDLER_ENTRIES`] (2026-10-07): its implicit
/// edge is the local-handler stub's jump to the entry's label, which never
/// passes the `NOP` either, and the walk compiles a live fall-through into it
/// only with the one reference the exception edge also brings
/// (`singlepass-codegen/local-handler-entry-live-fallthrough` refuses any
/// other depth), so the claim's map is exactly the fall-through edge's frame.
/// Nothing reads an `OsrExit` map at a handler's bci: the exception edge's
/// precise frames are `RETHROW` points keyed by the THROWING bci
/// (`exc_frame_box_ptr_by_bci`), and a handler-only pc publishes no OSR
/// entry. javac never falls into a handler (the protected code ends in a
/// `goto`, a return or `athrow`), so only generated bytecode reaches this.
pub(in crate::x64) fn forward_only_joins(
    code: &[u8],
    code_len: usize,
    branch_targets: &[bool],
    handler_pcs: &[usize],
    loop_boundary_maps: &rustc_hash::FxHashSet<usize>,
) -> Vec<bool> {
    let mut joins: Vec<bool> = (0..=code_len)
        .map(|pc| branch_targets.get(pc).copied().unwrap_or(false))
        .collect();
    if let Some(entry) = joins.get_mut(0) {
        *entry = false;
    }
    if !POST_CALL_EXIT_CLAIMS_AT_HANDLER_ENTRIES {
        for &h in handler_pcs {
            if let Some(j) = joins.get_mut(h) {
                *j = false;
            }
        }
    }
    for (target, _source) in crate::bytecode_analysis::back_edges(code, code_len) {
        if let Some(j) = joins.get_mut(target) {
            *j = false;
        }
    }
    for &header in loop_boundary_maps {
        if let Some(j) = joins.get_mut(header) {
            *j = false;
        }
    }
    joins
}

/// A post-call exit site after a real call inside a splice, queued by
/// [`Compiler::emit_splice_post_call_exit_site`]: the patchable `NOP`, the
/// identity box of its recorded chain point, and the source rows of the
/// spliced bodies its chain names (the innermost one, then every enclosing
/// spliced callee of a nested splice; a body with no class id to name it by
/// has none).
pub(crate) struct SplicePostCallExit {
    pub(in crate::x64) site: usize,
    pub(in crate::x64) point: *const crate::deopt::DeoptimizationPoint,
    pub(in crate::x64) sources: Vec<crate::SpliceScopeSource>,
}

/// One level of a NESTED single-pass splice, pushed by
/// `inlining.rs::try_emit_nested_inline` while the nested body is emitted
/// and popped on both of its exits (interpreter round i2, the i62-L2
/// nested-splice proposal, built behind
/// [`sp_nested_post_call_splice_exits_enabled`]).
///
/// A nested splice pushes no caller scope on `inline_scope_stack` (every
/// other point would inherit it); this is the scope a post-call exit INSIDE
/// the nested body needs, and only it reads it. `parked` is the ENCLOSING
/// spliced callee parked in its `invoke` of the nested one (arguments
/// popped, the result not yet pushed), built as the wave-59 site builds a
/// callee frame: its locals from the enclosing body's memory homes typed by
/// a whole-body kind scan, its operand window below the arguments all
/// references. `None` when that frame cannot be described, which refuses
/// every site inside this level.
pub(crate) struct NestedSpliceLevel {
    pub(in crate::x64) parked: Option<crate::deopt::FrameState>,
    /// The enclosing callee's source row, for the chain sinks to rebuild its
    /// scope after a redefinition (`None`: no class id).
    pub(in crate::x64) enclosing_source: Option<crate::SpliceScopeSource>,
    /// The NESTED callee's identity and geometry, as
    /// `InlineCalleeScope` gives them for the outermost splice: its key, its
    /// own `max_locals`, and where its window of the shared operand stack
    /// starts.
    pub(in crate::x64) key: String,
    pub(in crate::x64) max_locals: usize,
    pub(in crate::x64) caller_stack_floor: usize,
}

/// May the caller scope `push_inline_scope` captured be rebuilt at an exit
/// INSIDE the splice? Every value must still be where the scope says when
/// the body has run up to a call's return: a constant, a frame word, or a
/// callee-saved local home (`LOCAL_REGS`: RBX, R12-R15, and RSI / RDI on
/// Windows since wave 62; the splice body and every call it makes preserve
/// them, and the exit stub spills them). Never an XMM or a
/// caller-saved GPR (the body clobbers them), a scalar-replaced object, an
/// undescribed slot, or a held monitor (a door hands the method monitor to
/// ONE resumed frame; a chain's lock is not this exit's to move).
pub(in crate::x64::op_invoke) fn splice_caller_scope_is_transferable(fs: &crate::deopt::FrameState) -> bool {
    splice_caller_scope_refusal(fs).is_none()
}

/// One value of [`splice_caller_scope_is_transferable`]'s rule.
pub(in crate::x64::op_invoke) fn splice_scope_value_ok(v: &crate::deopt::FrameValue) -> bool {
    use crate::deopt::FrameValue;
    match v {
        FrameValue::Int(_)
        | FrameValue::Long(_)
        | FrameValue::Float(_)
        | FrameValue::Double(_)
        | FrameValue::Undefined
        | FrameValue::StackSlot(_)
        | FrameValue::StackSlotRef(_)
        | FrameValue::StackSlotLong(_)
        | FrameValue::StackSlotFloat(_)
        | FrameValue::StackSlotDouble(_) => true,
        FrameValue::Object(0) => true,
        // A callee-saved local home of this ABI (`LOCAL_REGS`: RBX and
        // R12-R15, and on Windows RSI and RDI too). Interpreter round i2 wave
        // 62, lane L2: the rule named RBX / R12-R15 only, which refused every
        // Windows caller scope with a local homed in RSI or RDI (measured on
        // `L2W62SinglePassOsrPostCallExitInsideASplice`'s OSR loops: `local 0 =
        // RegisterLong(6)`); the splice body never allocates a root local's
        // home, every call it makes preserves it, and the exit stub saves all
        // sixteen GPRs.
        FrameValue::Register(r) | FrameValue::RegisterLong(r) | FrameValue::RegisterRef(r) => {
            super::LOCAL_REGS.contains(r)
        }
        _ => false,
    }
}

/// [`splice_caller_scope_is_transferable`] naming the first thing it refuses
/// (interpreter round i2 wave 62, lane L2: the `CRATONVM_DBG_JITC` refusal
/// line of [`Compiler::emit_splice_post_call_exit_site`] prints it).
pub(in crate::x64::op_invoke) fn splice_caller_scope_refusal(fs: &crate::deopt::FrameState) -> Option<String> {
    if !fs.monitors.is_empty() {
        return Some("a held monitor".to_string());
    }
    if let Some((i, v)) = fs
        .locals
        .iter()
        .enumerate()
        .find(|(_, v)| !splice_scope_value_ok(v))
    {
        return Some(format!("local {i} = {v:?}"));
    }
    if let Some((i, v)) = fs
        .stack
        .iter()
        .enumerate()
        .find(|(_, v)| !splice_scope_value_ok(v))
    {
        return Some(format!("stack {i} = {v:?}"));
    }
    fs.caller.as_deref().and_then(splice_caller_scope_refusal)
}

/// The successors a CLAIMED post-call site admits (interpreter round i1
/// wave 48): constants, local loads and stores, the stack shuffles, `iinc`
/// and the field reads (the wave-46 list), a `goto` whatever its stack (a
/// live stack's `goto` files its own map at its bci, which the claim then
/// shares or yields to), and the instructions that list refused only because
/// their lowering MAY file a point at their own bci: array loads and stores,
/// field stores, arithmetic, conditional branches and switches, `ldc`, the
/// four ordinary invokes, the allocations and the type checks. Still refused:
/// the returns and `athrow` (nothing after them runs in this frame), and
/// `jsr` / `ret`.
///
/// `invokedynamic` and the monitor instructions are admitted under
/// [`POST_CALL_EXIT_CLAIMS_AT_INDY_AND_MONITORS`] (they were refused from
/// wave 48 until 2026-10-07): the claim is decided by what is filed at the
/// bci, like every other successor's. A BRIDGED `invokedynamic`
/// (`"v=" + f()`: `invokestatic f; invokedynamic makeConcatWithConstants`)
/// files no point of its own -- its bridge call's only point is the
/// `RETHROW` frame every claim ignores -- so the claim keeps its own map
/// there (`Own`). A TRAPPING one files its `UnreachedCode` snapshot keyed in
/// `osr_exit_box_ptr_by_bci` at that bci, so the claim is `Dropped` and
/// never shares it (a trap's map is charged as a failed speculation); the
/// frame then leaves at the trap itself, before the instruction runs, which
/// is the claim's own resume point. A `monitorenter` / `monitorexit`
/// successor re-executes in the interpreter with the frame the claim
/// describes (the lock it would take, or the hold it would release, is
/// exactly what [`Compiler::branch_mode_exit_admits`] checks for any
/// successor inside a `synchronized` block); javac never puts either right
/// after a call (it stores the lock object first), so this is hand-written
/// or generated bytecode only.
pub(in crate::x64::op_invoke) fn post_call_exit_claim_successor_admitted(op: u8) -> bool {
    match op {
        // invokedynamic.
        0xba => POST_CALL_EXIT_CLAIMS_AT_INDY_AND_MONITORS,
        // jsr, ret, jsr_w.
        0xa8 | 0xa9 | 0xc9 => false,
        // ireturn .. return, athrow.
        0xac..=0xb1 | 0xbf => false,
        // monitorenter, monitorexit.
        0xc2 | 0xc3 => POST_CALL_EXIT_CLAIMS_AT_INDY_AND_MONITORS,
        // Everything else a verified method can put after a call: nop ..
        // sipush, ldc .. ldc2_w, the loads, array loads, stores, array stores,
        // the stack shuffles, arithmetic, conversions, compares, the
        // conditional branches, goto, the switches, the field accesses, the
        // ordinary invokes, new .. arraylength, checkcast, instanceof, wide,
        // multianewarray, ifnull / ifnonnull, goto_w.
        0x00..=0xa7 | 0xaa | 0xab | 0xb2..=0xb9 | 0xbb..=0xbe | 0xc0 | 0xc1 | 0xc4..=0xc8 => true,
        _ => false,
    }
}

/// May an `OsrExit` map of `map`'s frame and `semantics` sit at the bci of
/// `guard`, a non-rethrow point of another reason already filed there
/// (interpreter round i1 wave 49, lane L2, behind
/// [`crate::osr_exit::OSR_EXIT_MAPS_BESIDE_GUARDS`];
/// `interpreter-L2-proposal-a-trap-is-charged-by-the-point-its-stub-baked-FIXED-20261013`)?
///
/// Only when the two describe the SAME interpreter state: the guard
/// re-executes its instruction as the map does (same `semantics`), and its
/// frame matches the map's value by value and location by location
/// ([`post_call_exit_frames_match`]). Then either stub resumes the same
/// frame, and each stashes the cause of the point it baked (a guard's stub
/// bakes `deopt_box_ptr_by_bci[(bci, reason)]`, the exit's
/// `osr_exit_box_ptr_by_bci[bci]`), so a trap is charged by what fired; the
/// cause-less by-bci lookup skips the map beside a guard
/// ([`crate::osr_exit::deopt_reason_at_bci`]). Never beside an `OsrExit`
/// point (the callers share or refuse those) nor an `UnreachedCode` one (the
/// `invokedynamic` trap snapshot is filed in the exit's own by-bci map).
pub(in crate::x64) fn guard_admits_an_exit_map_beside(
    guard: &crate::deopt::DeoptimizationPoint,
    map: &crate::deopt::FrameState,
    semantics: crate::deopt::ResumeSemantics,
) -> bool {
    use crate::deopt::DeoptReason;
    let admitted = crate::osr_exit::OSR_EXIT_MAPS_BESIDE_GUARDS
        && !matches!(
            guard.reason,
            DeoptReason::OsrExit | DeoptReason::UnreachedCode
        )
        && !guard.semantics.rethrow_exception
        && guard.semantics == semantics
        && post_call_exit_frames_match(&guard.frame_state, map);
    // JIT round 19 (lane selflock19, SYNCDOOR618-3): the census of the
    // `ResumeSemantics` compare. A refusal caused ONLY by the self-lock mark
    // (`method_monitor_first`, the one bit `resume_kind()` strips besides
    // `first_arrival`), with frames that match value by value, is a merge an
    // explicit `resume_kind()` + mark compare could have made. One
    // `CRATONVM_DBG_JITC` line per such refusal (`grep -c
    // 'self-lock-mark-merge'`); the proposal is built only if it is non-zero
    // on the blocks battery. Reporting only.
    if !admitted
        && crate::x64::dbg_jitc_enabled()
        && guard.semantics != semantics
        && guard.semantics.resume_kind() == semantics.resume_kind()
        && guard.semantics.first_arrival == semantics.first_arrival
        && !matches!(
            guard.reason,
            DeoptReason::OsrExit | DeoptReason::UnreachedCode
        )
        && !guard.semantics.rethrow_exception
        && post_call_exit_frames_match(&guard.frame_state, map)
    {
        eprintln!(
            "[cratonvm-jitc] self-lock-mark-merge refused site=exit-map-beside-guard {} bci={} \
             guard-mark={} map-mark={}",
            map.method_key,
            map.bci,
            guard.semantics.method_monitor_first,
            semantics.method_monitor_first,
        );
    }
    admitted
}

/// Do two frame states describe the same interpreter frame, value by value
/// and location by location? Two `OsrExit` maps at one bci may share one
/// point only then ([`Compiler::resolve_post_call_exit_claims`]).
pub(in crate::x64::op_invoke) fn post_call_exit_frames_match(a: &crate::deopt::FrameState, b: &crate::deopt::FrameState) -> bool {
    a.method_key == b.method_key
        && a.bci == b.bci
        && a.locals == b.locals
        && a.stack == b.stack
        && a.caller.is_none()
        && b.caller.is_none()
        && a.monitors.len() == b.monitors.len()
        && a.monitors.iter().zip(&b.monitors).all(|(x, y)| {
            x.object == y.object && x.lock_depth == y.lock_depth && x.relock == y.relock
        })
}

/// The first place two frame states differ, in the terms of
/// [`post_call_exit_frames_match`] (`None` when it would answer `true`): the
/// census line of `Compiler::report_post_call_header_claim_census`
/// (interpreter round i2 wave 61, lane L2). `header` is the loop header's
/// own map, `claim` the frame the call's return would describe.
pub(in crate::x64::op_invoke) fn first_frame_difference(
    header: &crate::deopt::FrameState,
    claim: &crate::deopt::FrameState,
) -> Option<String> {
    if header.method_key != claim.method_key || header.bci != claim.bci {
        return Some("identity".to_string());
    }
    if header.caller.is_some() || claim.caller.is_some() {
        return Some("caller-chain".to_string());
    }
    if header.locals.len() != claim.locals.len() {
        return Some(format!(
            "locals-len {} vs {}",
            header.locals.len(),
            claim.locals.len()
        ));
    }
    if let Some((i, (h, c))) = header
        .locals
        .iter()
        .zip(&claim.locals)
        .enumerate()
        .find(|(_, (h, c))| h != c)
    {
        return Some(format!("local {i}: header={h:?} call-return={c:?}"));
    }
    if header.stack.len() != claim.stack.len() {
        return Some(format!(
            "stack-len {} vs {}",
            header.stack.len(),
            claim.stack.len()
        ));
    }
    if let Some((i, (h, c))) = header
        .stack
        .iter()
        .zip(&claim.stack)
        .enumerate()
        .find(|(_, (h, c))| h != c)
    {
        return Some(format!("stack {i}: header={h:?} call-return={c:?}"));
    }
    if !post_call_exit_frames_match(header, claim) {
        return Some("monitors".to_string());
    }
    None
}

/// Bytes the shared verdict tail reserves below its realigned stack pointer:
/// the Win64 home area (32, harmless on SysV) and the sixteen XMM registers
/// (16 bytes each). A multiple of 16, so the `CALL` stays aligned.
pub(in crate::x64::op_invoke) const POST_CALL_EXIT_TAIL_FRAME: i32 = 0x20 + 16 * 16;

/// A single `NOP` instruction of `len` bytes (1 to 4; longer asks are clamped
/// to 4), the padding that moves a post-call exit site into one 8-aligned
/// word.
pub(in crate::x64::op_invoke) fn post_call_exit_pad(len: usize) -> &'static [u8] {
    match len {
        0 => &[],
        1 => &[0x90],
        2 => &[0x66, 0x90],
        3 => &[0x0F, 0x1F, 0x00],
        _ => &[0x0F, 0x1F, 0x40, 0x00],
    }
}

impl Compiler {
    /// After the invoke at the walk's current pc made a real call and left
    /// the walk at `next`: emit a post-call exit site there when one is
    /// admitted (see the section note above). Called by `compile_bytecode`
    /// only; emits nothing and files nothing on a refusal.
    ///
    /// Admitted in an OSR-tier compile (`mode_exit_polls`: the probe's loops,
    /// and a body that already has a loop-boundary exit map, so no caller-held
    /// direct `CALL` route that refuses a body with `osr_exit_points` loses
    /// anything) or, since interpreter round i1 wave 29 (lane L3), a
    /// method-entry compile that already filed such a map before the call
    /// ([`METHOD_ENTRY_POST_CALL_EXITS_ENABLED`]) -- since wave 39 also one
    /// that files the body's first such map here when it keeps a dispatch
    /// record ([`METHOD_ENTRY_FIRST_MAP_POST_CALL_EXITS_ENABLED`]) --, that reserved a compile
    /// id (the stub names its body to the verdict), outside a splice, and
    /// whatever [`Self::emit_post_call_exit_claim`] admits for `next`: the
    /// site is a CLAIM settled after the walk (interpreter round i1 wave 48;
    /// the wave-47 admission that filed its map at once was deleted in
    /// interpreter round i2 wave 62).
    pub(in crate::x64) fn emit_post_call_exit_site(
        &mut self,
        code: &[u8],
        code_len: usize,
        next: usize,
        branch_targets: &[bool],
    ) {
        self.emit_post_call_exit_claim(code, code_len, next, branch_targets);
    }

    /// After a `getstatic` / `putstatic` HELPER call (`op_field.rs`), standing
    /// at the instruction after it, `next`: the post-call exit claim an invoke
    /// gets ([`Self::emit_post_call_exit_site`]), when the access's class may
    /// still be uninitialized -- the VM did not prove it initialized at
    /// compile time (`DirectHelperTable::class_is_initialized`, `false` when
    /// it cannot tell), so the helper may run its `<clinit>`. An initialized
    /// class whose reads stay on the helper (`java/lang/System`, a second VM)
    /// gets no site. Under [`POST_CLINIT_HELPER_EXIT_SITES`].
    ///
    /// A `<clinit>` is Java code: it can redefine (or block while another
    /// thread redefines) a class this body spliced, and the helper then
    /// returned into the old splice and ran the rest of the activation on it
    /// (measured, 2026-10-07: `I24PostHelperRedefinition.java`
    /// rows `getstatic` / `putstatic` printed `false` in both tiers, HotSpot
    /// `true`: its compiled code reaches an uninitialized class through an
    /// uncommon trap or a patching stub, and a frame marked meanwhile is
    /// deoptimized on the return). The site is a `NOP` until a redefinition
    /// forces it, exactly as after an invoke; its map is at `next` with the
    /// loaded value pushed (`getstatic`) or the stored one popped
    /// (`putstatic`), the state the interpreter has after the access.
    pub(in crate::x64) fn emit_post_clinit_helper_exit_site(
        &mut self,
        code: &[u8],
        code_len: usize,
        next: usize,
        branch_targets: &[bool],
        class_id_raw: u32,
    ) {
        if !POST_CLINIT_HELPER_EXIT_SITES || self.direct_helpers.class_is_initialized(class_id_raw)
        {
            return;
        }
        self.emit_post_call_exit_claim(code, code_len, next, branch_targets);
    }

    /// The body of [`Self::emit_post_call_exit_site`]:
    /// the compile-wide conditions, `next` not a branch target (a loop
    /// header files its own map there, which OSR entry reads by bci) unless it
    /// is a forward-only join ([`forward_only_joins`], since wave 60: the
    /// site's `NOP` sits on the fall-through edge alone, before the join's
    /// label, so its map describes only the frame that edge brings), a successor [`post_call_exit_claim_successor_admitted`] admits,
    /// and the admission of [`Self::branch_mode_exit_target`] for a map at
    /// `next` with the whole current stack as its operands -- but instead of
    /// filing that map, the point is built here, at the call's return, and
    /// queued with the site's `NOP` as a [`PostCallExitClaim`] that
    /// [`Self::resolve_post_call_exit_claims`] settles after the walk, when
    /// every point the successor's own arm files at `next` is known. A
    /// rotated loop's entry `goto` and the constant of a fused backward
    /// compare need no refusal of their own (wave 47's): the pre-header
    /// guards are a point of another reason there (the claim is dropped), and
    /// the fused back edge's map is an `OsrExit` map the claim shares or
    /// yields to.
    pub(in crate::x64::op_invoke) fn emit_post_call_exit_claim(
        &mut self,
        code: &[u8],
        code_len: usize,
        next: usize,
        branch_targets: &[bool],
    ) {
        // Interpreter round i2 wave 60, lane L2: a branch-target successor is
        // admitted when it is a forward-only join
        // ([`post_call_exits_at_forward_joins_enabled`]).
        if !POST_CALL_EXITS_ENABLED
            || !self.post_call_exit_site_may_be_emitted()
            || next >= code_len
        {
            return;
        }
        let mut loop_header = false;
        if branch_targets.get(next).copied().unwrap_or(true)
            && !self
                .post_call_forward_joins
                .get(next)
                .copied()
                .unwrap_or(false)
        {
            // Wave 61: a LOOP HEADER successor, under its switch, for the
            // `Shared` outcome only (`post_call_exits_at_loop_headers_enabled`).
            // Not pc 0 (the method entry has no call before it anyway) and
            // only a textual back edge's target: a handler entry stays
            // refused.
            loop_header = next != 0
                && self
                    .post_call_loop_headers
                    .get(next)
                    .copied()
                    .unwrap_or(false);
            if !loop_header {
                // The census of a loop-header successor (debug only).
                if false {
                    self.note_post_call_header_claim_census(code, code_len, next);
                }
                return;
            }
        }
        let Some(&op) = code.get(next) else {
            return;
        };
        if !post_call_exit_claim_successor_admitted(op) {
            return;
        }
        let operands = self.stack.len();
        if !self.branch_mode_exit_admits(next, operands) {
            return;
        }
        let point = self.build_deopt_point(next, crate::deopt::DeoptReason::OsrExit);
        let site = self.emit_post_call_exit_patch_nop();
        // Interpreter round i6 wave 121, lane L2 (i43-L1 stage 3a): the call
        // this site follows, for `CompiledMethod::debugger_exit_call_bcis`.
        if self.debugger_observes_locals {
            self.debugger_claim_call_bcis.push((site, self.cur_bc_pc));
        }
        self.post_call_exit_claims.push(PostCallExitClaim {
            site,
            bci: next,
            point,
            loop_header,
        });
    }

    /// Interpreter round i6 wave 121, lane L2 (stage 3a of
    /// `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`):
    /// the claim at `site`, whose `OsrExit` map stands at `exit_bci`, was
    /// kept, so `(call bci, exit_bci)` joins [`Self::debugger_exit_call_bcis`].
    /// Nothing for a site the walk did not note (any compile without
    /// `debugger_observes_locals`).
    pub(in crate::x64::op_invoke) fn note_debugger_exit_claim_kept(&mut self, site: usize, exit_bci: usize) {
        if let Some(&(_, call_bci)) = self.debugger_claim_call_bcis.iter().find(|&&(s, _)| s == site)
        {
            if let (Ok(call_bci), Ok(exit_bci)) = (u32::try_from(call_bci), u32::try_from(exit_bci))
            {
                self.debugger_exit_call_bcis.push((call_bci, exit_bci));
            }
        }
    }

    /// Step 0 of
    /// `docs/feature-designs/jit-ir-optimizations.md`
    /// (interpreter round i2 wave 61, lane L2), under `CRATONVM_DBG_DEOPT`
    /// only: a claim refused because its successor `next` is a branch target
    /// that is no forward-only join. When `next` is a LOOP HEADER (a textual
    /// back edge's target) whose successor and admission would pass, the
    /// frame the claim would describe is built -- nothing is emitted, recorded
    /// or filed (`build_frame_state_at` reads the walk's state only) -- and
    /// kept for [`Self::report_post_call_header_claim_census`], which compares
    /// it with the header's own `OsrExit` map after the walk. Any other
    /// refusal prints its reason at once.
    pub(in crate::x64::op_invoke) fn note_post_call_header_claim_census(&mut self, code: &[u8], code_len: usize, next: usize) {
        let is_header = is_back_edge_target(code, code_len, next);
        let refusal = if !is_header {
            Some("not-a-loop-header")
        } else if !code
            .get(next)
            .copied()
            .is_some_and(post_call_exit_claim_successor_admitted)
        {
            Some("successor-refused")
        } else {
            let operands = self.stack.len();
            self.branch_mode_exit_refusal(next, operands)
        };
        if let Some(why) = refusal {
            eprintln!(
                "[cratonvm-deopt] post-call header claim census at bci={next} of {}: refused ({why})",
                self.method_key
            );
            return;
        }
        // Built exactly as a claim's point is (`emit_post_call_exit_claim`),
        // and like it not recorded.
        let point = self.build_deopt_point(next, crate::deopt::DeoptReason::OsrExit);
        self.post_call_header_claim_census.push(PostCallExitClaim {
            site: 0,
            bci: next,
            point,
            loop_header: true,
        });
    }

    /// After the walk, under `CRATONVM_DBG_DEOPT`: for every claim
    /// [`Self::note_post_call_header_claim_census`] kept, print whether the
    /// header's own `OsrExit` map (the one `osr_exit_box_ptr_by_bci` keys
    /// there) describes the same frame value by value and location by
    /// location -- the `Shared` outcome the proposal would admit -- and if
    /// not, the first slot that differs. Rank with
    /// `grep 'post-call header claim census' | sed 's/.*: //' | sort | uniq -c`.
    pub(in crate::x64) fn report_post_call_header_claim_census(&mut self) {
        let census = std::mem::take(&mut self.post_call_header_claim_census);
        for claim in census {
            let keyed = self.osr_exit_box_ptr_by_bci.get(&claim.bci).copied();
            let header = keyed.and_then(|k| {
                self.deopt_boxes
                    .iter()
                    .position(|b| std::ptr::addr_of!(**b) == k)
                    .and_then(|i| self.deopt_points.get(i))
            });
            let verdict = match header {
                None => "no-header-map".to_string(),
                Some(h) if h.semantics != claim.point.semantics => "differ (semantics)".to_string(),
                Some(h) => match first_frame_difference(&h.frame_state, &claim.point.frame_state) {
                    None => "match".to_string(),
                    Some(slot) => format!("differ ({slot})"),
                },
            };
            eprintln!(
                "[cratonvm-deopt] post-call header claim census at bci={} of {}: {verdict}",
                claim.bci, self.method_key
            );
        }
    }

    /// Settle every [`PostCallExitClaim`] of the walk, before the stubs are
    /// laid down:
    /// every point the walk will file is filed by now (the successor arms',
    /// the loop pre-headers', the constant-pool helper sites'), so each claim
    /// is judged by what is actually at its bci
    /// ([`PostCallExitClaimOutcome`]). A claim that keeps a site queues it
    /// for [`Self::emit_post_call_exit_stubs`] exactly as a wave-47 site
    /// was; its stub's `JNZ` resolves to the reason-7 stub of the ONE
    /// `OsrExit` map at its bci, its own or the shared one. Under
    /// `CRATONVM_DBG_DEOPT` each claim prints its outcome (the positive
    /// control of `L2W48PostCallExitClaims`).
    pub(in crate::x64) fn resolve_post_call_exit_claims(&mut self) {
        let claims = std::mem::take(&mut self.post_call_exit_claims);
        // Wave 55: read once per compile that has a claim; method-entry
        // compiles only (the proposal's first stage keeps the OSR door, whose
        // transfer and exit classification read `OsrExit` maps by bci, out).
        // Wave 56: an OSR-tier compile too, under its own switch, once it
        // filed a loop-boundary or back-edge map (`can_osr_exit`, so the OSR
        // door transfers the exit; the door's by-bci readers learn the point
        // from `CompiledMethod::post_call_own_exit_bcis`).
        let own_point = !claims.is_empty()
            && (!self.mode_exit_polls
                || (!self.osr_exit_points.is_empty() && osr_post_call_own_point_exits_enabled()))
            && crate::deopt_real_enabled()
            && post_call_own_point_exits_enabled();
        for claim in claims {
            if self.failed {
                return;
            }
            let outcome = self.post_call_exit_claim_outcome(&claim, own_point);
            if false {
                let there: Vec<String> = self
                    .deopt_point_pcs
                    .iter()
                    .zip(&self.deopt_points)
                    .filter(|(&at, _)| at == claim.bci)
                    .map(|(_, p)| format!("{:?}/{}", p.reason, p.semantics))
                    .collect();
                eprintln!(
                    "[cratonvm-deopt] post-call exit claim at bci={} of {}: {outcome:?} \
                     (points there: {there:?})",
                    claim.bci, self.method_key,
                );
            }
            match outcome {
                PostCallExitClaimOutcome::Own => {
                    let bci = claim.bci;
                    let site = claim.site;
                    let box_ptr = self.record_built_deopt_point_in_order(bci, claim.point);
                    self.file_osr_exit_box(bci, box_ptr);
                    // Keyed only outside a splice, where every claim was made;
                    // checked anyway so a missing key can never bake the shared
                    // exit into this site's stub.
                    if self.osr_exit_box_ptr_by_bci.get(&bci).copied() == Some(box_ptr) {
                        self.post_call_exits_pending.push((site, bci));
                        self.note_debugger_exit_claim_kept(site, bci);
                    }
                }
                PostCallExitClaimOutcome::Shared => {
                    self.post_call_exits_pending.push((claim.site, claim.bci));
                    self.note_debugger_exit_claim_kept(claim.site, claim.bci);
                }
                PostCallExitClaimOutcome::OwnBeside => {
                    let bci = claim.bci;
                    let site = claim.site;
                    // Recorded (sorted by native offset), never filed by bci:
                    // the stub finds it through `post_call_own_box_ptr_by_bci`.
                    let box_ptr = self.record_built_deopt_point_in_order(bci, claim.point);
                    self.post_call_own_box_ptr_by_bci.insert(bci, box_ptr);
                    self.post_call_exits_pending.push((site, bci));
                    self.note_debugger_exit_claim_kept(site, bci);
                }
                PostCallExitClaimOutcome::Dropped => {}
            }
        }
    }

    /// Judge one claim against the points filed at its bci: the rethrow
    /// points (a `PendingException` frame, which neither by-bci reason lookup
    /// confuses with an `OsrExit` one) are ignored, and since wave 49 so are
    /// `REEXECUTE` guards of the claim's own frame
    /// ([`guard_admits_an_exit_map_beside`]); none left is
    /// [`PostCallExitClaimOutcome::Own`]; exactly one, the `OsrExit` map
    /// `osr_exit_box_ptr_by_bci` names, describing the claim's frame, is
    /// [`PostCallExitClaimOutcome::Shared`]; anything else is
    /// [`PostCallExitClaimOutcome::Dropped`].
    pub(in crate::x64::op_invoke) fn post_call_exit_claim_outcome(
        &self,
        claim: &PostCallExitClaim,
        own_point: bool,
    ) -> PostCallExitClaimOutcome {
        if claim.loop_header {
            // Wave 61: a loop header's map is the header's entry contract;
            // the claim may only share it.
            return match self.post_call_exit_claim_outcome_by_bci(claim) {
                PostCallExitClaimOutcome::Shared => PostCallExitClaimOutcome::Shared,
                _ => PostCallExitClaimOutcome::Dropped,
            };
        }
        match self.post_call_exit_claim_outcome_by_bci(claim) {
            PostCallExitClaimOutcome::Dropped
                if own_point && self.claim_may_bake_its_own_point(claim) =>
            {
                PostCallExitClaimOutcome::OwnBeside
            }
            outcome => outcome,
        }
    }

    /// May a claim the by-bci rule drops keep its own point beside what is
    /// at its bci ([`PostCallExitClaimOutcome::OwnBeside`])? Only when no
    /// `OsrExit` map is keyed there (no loop-boundary, back-edge or
    /// `invokedynamic` map a reader could take for this one), no other claim
    /// baked its own point there, and every non-rethrow point there is a
    /// guard of another reason that resumes as the claim's map does
    /// (`REEXECUTE`, same `resume_kind`, so `osr_exit::resume_image` stays
    /// unique at the bci). The guards keep their own stubs and frames; a
    /// cause-less by-bci lookup charges them, never this map
    /// (`osr_exit::trap_reason_at_bci`).
    pub(in crate::x64::op_invoke) fn claim_may_bake_its_own_point(&self, claim: &PostCallExitClaim) -> bool {
        use crate::deopt::DeoptReason;
        if self.osr_exit_box_ptr_by_bci.contains_key(&claim.bci)
            || self.post_call_own_box_ptr_by_bci.contains_key(&claim.bci)
        {
            return false;
        }
        let mut any = false;
        for (&at, p) in self.deopt_point_pcs.iter().zip(&self.deopt_points) {
            if at != claim.bci || p.semantics.rethrow_exception {
                continue;
            }
            if matches!(p.reason, DeoptReason::OsrExit | DeoptReason::UnreachedCode)
                || p.semantics.resume_kind() != claim.point.semantics.resume_kind()
            {
                return false;
            }
            any = true;
        }
        any
    }

    /// [`Self::post_call_exit_claim_outcome`] by the by-bci rule of waves
    /// 48-49 alone: `Own`, `Shared` or `Dropped`.
    pub(in crate::x64::op_invoke) fn post_call_exit_claim_outcome_by_bci(
        &self,
        claim: &PostCallExitClaim,
    ) -> PostCallExitClaimOutcome {
        use crate::deopt::DeoptReason;
        let mut others = self
            .deopt_point_pcs
            .iter()
            .enumerate()
            .filter(|(_, &at)| at == claim.bci)
            .filter_map(|(i, _)| Some((i, self.deopt_points.get(i)?)))
            .filter(|(_, p)| !p.semantics.rethrow_exception)
            // Wave 49: a guard of the claim's own frame is no obstacle
            // (`guard_admits_an_exit_map_beside`, false with the switch off).
            .filter(|(_, p)| {
                !guard_admits_an_exit_map_beside(p, &claim.point.frame_state, claim.point.semantics)
            });
        let Some((i, other)) = others.next() else {
            return if self.osr_exit_box_ptr_by_bci.contains_key(&claim.bci) {
                // A key with no point behind it: never shared, never replaced.
                PostCallExitClaimOutcome::Dropped
            } else {
                PostCallExitClaimOutcome::Own
            };
        };
        if others.next().is_some() || other.reason != DeoptReason::OsrExit {
            return PostCallExitClaimOutcome::Dropped;
        }
        let keyed = self.osr_exit_box_ptr_by_bci.get(&claim.bci).copied();
        let boxed = self.deopt_boxes.get(i).map(|b| std::ptr::addr_of!(**b));
        if keyed.is_none() || keyed != boxed {
            return PostCallExitClaimOutcome::Dropped;
        }
        if other.semantics == claim.point.semantics
            && post_call_exit_frames_match(&other.frame_state, &claim.point.frame_state)
        {
            PostCallExitClaimOutcome::Shared
        } else {
            PostCallExitClaimOutcome::Dropped
        }
    }

    /// The compile-wide conditions of a post-call exit site, shared with the
    /// constant-pool helper exit sites: the body's tier admits one (an
    /// OSR-tier compile, or a method-entry one under the wave-29 / wave-39
    /// rules in [`Self::emit_post_call_exit_site`]), it reserved a compile id
    /// (the stub names its body to the verdict), the compile has room under
    /// [`POST_CALL_EXIT_SITES_MAX`], and the walk is outside a splice.
    pub(in crate::x64::op_invoke) fn post_call_exit_site_may_be_emitted(&self) -> bool {
        !self.failed
            && self.post_call_exit_tier_admitted()
            && self.compile_id.id() != 0
            && self.post_call_exit_sites_queued() < POST_CALL_EXIT_SITES_MAX
            && self.inline_callee_scopes.is_empty()
    }

    /// The tier half of [`Self::post_call_exit_site_may_be_emitted`]: an
    /// OSR-tier compile, or a method-entry one under the wave-29 / wave-39
    /// rules.
    pub(in crate::x64::op_invoke) fn post_call_exit_tier_admitted(&self) -> bool {
        self.mode_exit_polls
            || (METHOD_ENTRY_POST_CALL_EXITS_ENABLED
                && (!self.osr_exit_points.is_empty()
                    || (METHOD_ENTRY_FIRST_MAP_POST_CALL_EXITS_ENABLED
                        && (self.keeps_a_call_record()
                            || (METHOD_ENTRY_UNSYNCHRONIZED_FIRST_MAP_POST_CALL_EXITS
                                && self.method_known_unsynchronized)))))
    }

    /// Every post-call exit site the walk queued so far, of every kind, for
    /// [`POST_CALL_EXIT_SITES_MAX`].
    pub(in crate::x64::op_invoke) fn post_call_exit_sites_queued(&self) -> usize {
        self.post_call_exits_pending.len()
            + self.post_call_exit_claims.len()
            + self.splice_post_call_exits.len()
    }

    /// After a REAL call the splice walk emitted inside a spliced body
    /// (`inlining.rs`, the invoke arm's `emit_inline_invoke`), with the walk
    /// standing at the callee's successor pc `next`: a post-call exit site
    /// whose `OsrExit` point leaves through a two-frame CHAIN -- the spliced
    /// callee at `next` with the call's result pushed, over the compile's own
    /// method parked in its `invoke` of the callee. Interpreter round i2 wave
    /// 59, lane L2: the single-pass stage of
    /// `docs/feature-designs/deopt-osr.md`,
    /// behind [`sp_post_call_splice_exits_enabled`].
    ///
    /// The three pieces the wave-54 triage named, and what each is here:
    ///
    /// * **a chain frame-state producer for the single-pass walk.** The
    ///   caller scope is the one `push_inline_scope` captured at the splice
    ///   (the compile's own method at its invoke, arguments popped, built by
    ///   `build_frame_state_at` like any root point; under the switch the
    ///   splice flushes the scratch registers BEFORE capturing it, so no
    ///   caller operand names a register the body clobbers). The callee frame
    ///   is built here from what IS known about a spliced body: its locals
    ///   are memory-homed at `callee_local_base + 8 i` and zeroed at the
    ///   splice (`try_emit_inline_body`), so a whole-body kind scan
    ///   (`bce::classify_local_kinds`) types each one exactly -- a slot never
    ///   written yet reads `0`/null, which is what an unassigned slot the
    ///   interpreter can never read may hold; a slot the scan calls
    ///   `Ambiguous` refuses the site. Its operand stack below the result
    ///   must be references (their oop marks are the width source); the
    ///   result is typed by the call's descriptor.
    /// * **a point key that is not a bci.** Nothing else files a point inside
    ///   a splice (the by-bci key space belongs to the compile's own method,
    ///   `may_file_by_bci`), so the point is recorded in `deopt_points` only
    ///   and its stub (tag [`SPLICE_POST_CALL_STUB_TAG`]) bakes it through
    ///   `splice_post_call_box_ptrs`, by index.
    /// * **a single-pass chain consumer.** None needed: the stash sinks
    ///   (the method-entry doors, the call-site service by point) rebuild a
    ///   chain from any artifact's point, by its `splice_scope_class_ids` /
    ///   `splice_scope_sources`, which this compile now keeps for the bodies
    ///   its points name (`driver.rs`); and the VM's verdict asks
    ///   `post_call_chain_exits_resumable` first, as for the optimizing tier.
    ///
    /// Admitted only where every piece is certain: a METHOD-ENTRY compile --
    /// or, since wave 62 under [`sp_osr_post_call_splice_exits_enabled`], an
    /// OSR-tier one that already filed a loop-boundary exit map, whose chain
    /// exit the OSR door transfers by its recorded point
    /// (`deopt_resume::transfer_osr_exit_chain_into_live_frame`, as for any
    /// chain guard of a single-pass splice) -- of an unsynchronized method that is
    /// no self-locking body, with a compile id, the deopt register region and
    /// `CRATONVM_DEOPT_REAL`; no loop rewrite; the OUTERMOST splice only (a
    /// nested splice pushes no scope: `try_emit_nested_inline`); `next` inside
    /// the callee and not one of its branch targets; under the site cap.
    /// Anything else emits nothing and files nothing.
    pub(in crate::x64) fn emit_splice_post_call_exit_site(
        &mut self,
        site: &crate::InlineSite,
        next: usize,
        callee_branch_targets: &[bool],
        return_type: u8,
    ) {
        use crate::deopt::{FrameState, FrameValue};
        // Interpreter round i2 wave 60, lane L2: the census the "Still open"
        // shapes of the i39-L3 proposal wait for -- a real call inside a
        // NESTED single-pass splice, or inside a splice of a single-pass
        // OSR-tier compile. One `CRATONVM_DBG_JITC` line per such call, so a
        // run can count them (`grep sp-splice-post-call-exit-census | sort |
        // uniq -c`). Compile time only.
        // Wave 62: an OSR-tier compile is admitted under its own switch once it
        // filed a loop-boundary exit map, so the artifact is `can_osr_exit` and
        // the OSR door's chain transfer takes the exit
        // ([`sp_osr_post_call_splice_exits_enabled`]).
        let osr_tier_refused = self.mode_exit_polls
            && (self.osr_exit_points.is_empty() || !sp_osr_post_call_splice_exits_enabled());
        if dbg_jitc_enabled() && sp_post_call_splice_exits_enabled() {
            // A nested splice pushes a callee-local oop scope of its own but no
            // caller scope (`try_emit_nested_inline`).
            // Since the nested-splice stage, a nested call whose levels are
            // all describable gets its site and is no longer counted.
            let shape = if (self.inline_oop_scopes.len() > 1 || self.inline_callee_scopes.len() > 1)
                && !self.splice_scopes_admit_a_post_call_site()
            {
                Some("nested-splice")
            } else if osr_tier_refused && self.inline_callee_scopes.len() == 1 {
                Some("osr-tier-splice")
            } else {
                None
            };
            if let Some(shape) = shape {
                eprintln!(
                    "[cratonvm-jitc] sp-splice-post-call-exit-census {shape} outermost-callee={} in {}",
                    self.inline_callee_scopes.first().map_or("?", |s| s.key.as_str()),
                    self.method_key,
                );
            }
        }
        if !sp_post_call_splice_exits_enabled()
            || !POST_CALL_EXITS_ENABLED
            || !crate::deopt_real_enabled()
            || self.failed
            || osr_tier_refused
            || !self.post_call_exit_tier_admitted()
            || self.compile_id.id() == 0
            || self.post_call_exit_sites_queued() >= POST_CALL_EXIT_SITES_MAX
            || !self.splice_scopes_admit_a_post_call_site()
            || !self.method_known_unsynchronized
            || self.self_lock_obj_off > 0
            || self.bci_provenance.is_some()
            || self.deopt_regs_base == 0
            || next >= site.callee_code_len
            || callee_branch_targets.get(next).copied().unwrap_or(true)
        {
            // Wave 62: an admitted OSR-tier compile names the compile-wide
            // condition that still refused it (debug only, compile time).
            if self.mode_exit_polls && !osr_tier_refused && dbg_jitc_enabled() {
                let why = if self.bci_provenance.is_some() {
                    "loop-rewrite"
                } else if self.deopt_regs_base == 0 {
                    "no-deopt-register-region"
                } else if !self.method_known_unsynchronized || self.self_lock_obj_off > 0 {
                    "synchronized"
                } else if !self.splice_scopes_admit_a_post_call_site() {
                    "not-the-outermost-splice"
                } else if next >= site.callee_code_len
                    || callee_branch_targets.get(next).copied().unwrap_or(true)
                {
                    "successor"
                } else {
                    "compile-wide"
                };
                eprintln!(
                    "[cratonvm-jitc] sp-splice-post-call-exit REFUSED osr-tier {why}: callee={} bci={next} in {}",
                    self.inline_callee_scopes.first().map_or("?", |s| s.key.as_str()),
                    self.method_key,
                );
            }
            return;
        }
        // The frame as the `NOP` sees it: every operand in its frame word.
        self.flush_scratch_registers();
        // The innermost spliced callee: the outermost splice's scope, or the
        // innermost nested level's.
        let (key, max_locals, floor) = match self.sp_nested_splice_levels.last() {
            Some(level) => (
                level.key.clone(),
                level.max_locals,
                level.caller_stack_floor,
            ),
            None => {
                let Some(scope) = self.inline_callee_scopes.first() else {
                    return;
                };
                (
                    scope.key.clone(),
                    scope.max_locals,
                    scope.caller_stack_floor,
                )
            }
        };
        let Some(local_base) = self.inline_oop_scopes.last().map(|s| s.local_base) else {
            return;
        };
        let refuse = |why: &str, me: &Self| {
            if dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] sp-splice-post-call-exit REFUSED {why}: callee={key} bci={next} in {}",
                    me.method_key,
                );
            }
        };
        let code_len = site.callee_code_len.min(site.callee_code.len());
        let kinds = super::bce::classify_local_kinds(&site.callee_code, code_len, max_locals);
        let mut locals = Vec::with_capacity(max_locals);
        for (i, &kind) in kinds.iter().enumerate() {
            // Cast: a local index of a spliced body (`max_locals` <= 65535).
            let off = local_base + (i as i32) * 8;
            let fv = match kind {
                super::LocalKind::Ref => FrameValue::StackSlotRef(-off),
                other => super::typed_local_frame_value(None, None, off, other),
            };
            if matches!(fv, FrameValue::Unsupported) {
                refuse("a callee local of no single kind", self);
                return;
            }
            locals.push(fv);
        }
        // The callee's own window of the shared operand stack.
        let n = self.stack.len();
        if floor > n || self.stack_oop_marks.len() != n {
            refuse("operand stack window", self);
            return;
        }
        let mut stack = Vec::with_capacity(n - floor);
        for i in floor..n {
            let StackSlot::Frame(off) = self.stack[i] else {
                refuse("an operand outside its frame word", self);
                return;
            };
            let is_oop = self.stack_oop_marks[i];
            let fv = if i + 1 == n && return_type != b'V' {
                match return_type {
                    b'L' | b'[' if is_oop => FrameValue::StackSlotRef(-off),
                    b'L' | b'[' => {
                        refuse("an unmarked reference result", self);
                        return;
                    }
                    _ if is_oop => {
                        refuse("a primitive result marked an oop", self);
                        return;
                    }
                    b'J' => FrameValue::StackSlotLong(-off),
                    b'D' => FrameValue::StackSlotDouble(-off),
                    b'F' => FrameValue::StackSlotFloat(-off),
                    _ => FrameValue::StackSlot(-off),
                }
            } else if is_oop {
                FrameValue::StackSlotRef(-off)
            } else {
                // No width source for a primitive below the result.
                refuse("a primitive operand below the result", self);
                return;
            };
            stack.push(fv);
        }
        let Some(mut caller) = self.inline_caller_chain() else {
            return;
        };
        // A nested splice's enclosing callees, outermost first, each parked
        // in its `invoke` of the next ([`NestedSpliceLevel`]).
        for level in &self.sp_nested_splice_levels {
            let Some(parked) = level.parked.as_ref() else {
                return;
            };
            let mut fs = parked.clone();
            fs.caller = Some(caller);
            caller = Box::new(fs);
        }
        if !splice_caller_scope_is_transferable(&caller) {
            let why = splice_caller_scope_refusal(&caller).unwrap_or_default();
            refuse(&format!("the caller scope ({why})"), self);
            return;
        }
        // Cast: a callee bci (< 65536).
        let bci = next as u32;
        let reason = crate::deopt::DeoptReason::OsrExit;
        let point = crate::deopt::DeoptimizationPoint {
            // Cast: buffer position to encoding offset.
            native_offset: self.buf.pos() as u32,
            bci,
            reason,
            action: crate::deopt::DeoptAction::Reinterpret,
            semantics: crate::deopt::ResumeSemantics::for_reason(reason),
            speculation_id: crate::deopt::speculation_id(bci, reason),
            frame_state: FrameState {
                class_id: 0,
                method_key: key.clone(),
                bci,
                locals,
                stack,
                monitors: Vec::new(),
                caller: Some(caller),
            },
        };
        // Recorded under an emitter pc no instruction of THIS compile's own
        // method has: `next` is a CALLEE pc, and every by-pc reader of
        // `deopt_point_pcs` (the post-call claims' "what else is at my bci",
        // the back-edge admissions' "a point already there") asks about the
        // root method's pcs. Read by the loop-rewrite translation check only,
        // which never runs here (`bci_provenance` refused above).
        let box_ptr = self.record_built_deopt_point(SPLICE_POINT_EMITTER_PC, point);
        let nop = self.emit_post_call_exit_patch_nop();
        let mut padded = Vec::with_capacity(code_len + 2);
        padded.extend_from_slice(site.callee_code.get(..code_len).unwrap_or_default());
        padded.extend_from_slice(&[0, 0]);
        let source = (site.class_id != 0)
            .then(|| u16::try_from(site.callee_max_locals).ok())
            .flatten()
            .map(|max_locals| crate::SpliceScopeSource {
                method_key: key.clone(),
                class_id: site.class_id,
                code: std::sync::Arc::from(padded.into_boxed_slice()),
                max_locals,
                is_static: site.callee_is_static,
                // shortcut: the single-pass tier splices no body with a table (`!ir_mode` refuses it).
                exception_table: Vec::new(),
            });
        let sources = source
            .into_iter()
            .chain(
                self.sp_nested_splice_levels
                    .iter()
                    .filter_map(|level| level.enclosing_source.clone()),
            )
            .collect();
        self.splice_post_call_exits.push(SplicePostCallExit {
            site: nop,
            point: box_ptr,
            sources,
        });
        if dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] sp-splice-post-call-exit site nop={nop} callee={key} bci={next} in {}",
                self.method_key,
            );
        }
    }

    /// Do the open splices admit a post-call exit site inside the innermost
    /// one? The outermost splice's caller scope (exactly one), and either no
    /// nested level or, under [`sp_nested_post_call_splice_exits_enabled`], a
    /// describable [`NestedSpliceLevel`] for every nested level open.
    pub(in crate::x64::op_invoke) fn splice_scopes_admit_a_post_call_site(&self) -> bool {
        self.inline_callee_scopes.len() == 1
            && self.inline_scope_stack.len() == 1
            && self.inline_oop_scopes.len() == 1 + self.sp_nested_splice_levels.len()
            && (self.sp_nested_splice_levels.is_empty()
                || (sp_nested_post_call_splice_exits_enabled()
                    && self
                        .sp_nested_splice_levels
                        .iter()
                        .all(|level| level.parked.is_some())))
    }

    /// At a NESTED splice of `nested` inside the spliced body of `enclosing`,
    /// standing at the enclosing body's `invoke` with the arguments still on
    /// the operand stack: the [`NestedSpliceLevel`] a post-call exit site
    /// inside the nested body needs, or `None` (and nothing to push) when no
    /// such site could be admitted anyway (the switches, a compile the site's
    /// compile-wide rules refuse, a level that is not the next one down).
    /// Pushed by the caller (`inlining.rs::try_emit_nested_inline`), which
    /// pops it on both exits.
    ///
    /// The enclosing callee's frame is built as the wave-59 site builds a
    /// callee frame ([`Self::emit_splice_post_call_exit_site`]): every
    /// operand in its frame word (the nested body's first act is the same
    /// flush), its locals from its memory homes typed by
    /// `bce::classify_local_kinds` over its whole code (an `Ambiguous` local
    /// refuses), and its window of the operand stack below the arguments all
    /// references (their oop marks are the only width source). Its bci is the
    /// `invoke` itself; the VM computes the successor.
    pub(in crate::x64) fn capture_nested_splice_level(
        &mut self,
        enclosing: &crate::InlineSite,
        nested: &crate::InlineSite,
        enclosing_invoke_pc: usize,
    ) -> Option<NestedSpliceLevel> {
        use crate::deopt::{FrameState, FrameValue};
        if !sp_nested_post_call_splice_exits_enabled()
            || !sp_post_call_splice_exits_enabled()
            || self.failed
            || self.inline_callee_scopes.len() != 1
            || self.inline_scope_stack.len() != 1
            || self.inline_oop_scopes.len() != 1 + self.sp_nested_splice_levels.len()
            || !self.method_known_unsynchronized
            || self.self_lock_obj_off > 0
            || self.compile_id.id() == 0
        {
            return None;
        }
        // The enclosing callee's identity and geometry: the outermost
        // splice's scope, or the innermost nested level's.
        let (enc_key, enc_max_locals, enc_floor) = match self.sp_nested_splice_levels.last() {
            Some(level) => (
                level.key.clone(),
                level.max_locals,
                level.caller_stack_floor,
            ),
            None => {
                let scope = self.inline_callee_scopes.first()?;
                (
                    scope.key.clone(),
                    scope.max_locals,
                    scope.caller_stack_floor,
                )
            }
        };
        let local_base = self.inline_oop_scopes.last()?.local_base;
        let key = format!(
            "{}.{}:{}",
            nested.class_name, nested.method_name, nested.descriptor
        );
        let n = self.stack.len();
        let caller_stack_floor = n.checked_sub(nested.callee_num_args)?;
        let describable = self
            .sp_nested_splice_levels
            .iter()
            .all(|l| l.parked.is_some())
            && enc_floor <= caller_stack_floor
            && self.stack_oop_marks.len() == n;
        let parked = if describable {
            self.flush_scratch_registers();
            let code_len = enclosing.callee_code_len.min(enclosing.callee_code.len());
            let kinds =
                super::bce::classify_local_kinds(&enclosing.callee_code, code_len, enc_max_locals);
            let mut locals = Vec::with_capacity(enc_max_locals);
            for (i, &kind) in kinds.iter().enumerate() {
                // Cast: a local index of a spliced body (`max_locals` <= 65535).
                let off = local_base + (i as i32) * 8;
                let fv = match kind {
                    super::LocalKind::Ref => FrameValue::StackSlotRef(-off),
                    other => super::typed_local_frame_value(None, None, off, other),
                };
                locals.push(fv);
            }
            let mut stack = Vec::with_capacity(caller_stack_floor - enc_floor);
            for i in enc_floor..caller_stack_floor {
                stack.push(match self.stack[i] {
                    StackSlot::Frame(off) if self.stack_oop_marks[i] => {
                        FrameValue::StackSlotRef(-off)
                    }
                    _ => FrameValue::Unsupported,
                });
            }
            let complete = !locals
                .iter()
                .chain(stack.iter())
                .any(|v| matches!(v, FrameValue::Unsupported));
            // Cast: a callee bci (< 65536).
            complete.then(|| FrameState {
                class_id: 0,
                method_key: enc_key.clone(),
                bci: enclosing_invoke_pc as u32,
                locals,
                stack,
                monitors: Vec::new(),
                caller: None,
            })
        } else {
            None
        };
        if parked.is_none() && dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] sp-nested-splice-level UNDESCRIBABLE: {enc_key} at bci={enclosing_invoke_pc} splicing {key} in {}",
                self.method_key,
            );
        }
        let code_len = enclosing.callee_code_len.min(enclosing.callee_code.len());
        let enclosing_source = (parked.is_some() && enclosing.class_id != 0)
            .then(|| u16::try_from(enclosing.callee_max_locals).ok())
            .flatten()
            .map(|max_locals| {
                let mut padded = Vec::with_capacity(code_len + 2);
                padded.extend_from_slice(enclosing.callee_code.get(..code_len).unwrap_or_default());
                padded.extend_from_slice(&[0, 0]);
                crate::SpliceScopeSource {
                    method_key: enc_key.clone(),
                    class_id: enclosing.class_id,
                    code: std::sync::Arc::from(padded.into_boxed_slice()),
                    max_locals,
                    is_static: enclosing.callee_is_static,
                    exception_table: Vec::new(),
                }
            });
        Some(NestedSpliceLevel {
            parked,
            enclosing_source,
            key,
            max_locals: nested.callee_max_locals,
            caller_stack_floor,
        })
    }

    /// Emit the patchable five-byte `NOP` of a site whose `OsrExit` map
    /// `branch_mode_exit_target` just recorded at `exit_bci`, and queue its
    /// stub for [`Self::emit_post_call_exit_stubs`].
    pub(in crate::x64::op_invoke) fn emit_post_call_exit_nop(&mut self, exit_bci: usize) {
        let site = self.emit_post_call_exit_patch_nop();
        self.post_call_exits_pending.push((site, exit_bci));
    }

    /// The patchable five-byte `NOP` of a post-call exit site, aligned so it
    /// sits inside one 8-aligned word; returns its buffer offset.
    pub(in crate::x64::op_invoke) fn emit_post_call_exit_patch_nop(&mut self) -> usize {
        // Five bytes inside one 8-aligned word, so the force is one aligned
        // store (`not_entrant::post_call_exit_jump` checks it again).
        // Cast: the site's absolute address, for its alignment only.
        let here = self.buf.as_ptr() as usize + self.buf.pos();
        let misalign = here % 8;
        if misalign > 8 - crate::ENTRY_PATCH_PAD.len() {
            self.buf.emit(post_call_exit_pad(8 - misalign));
        }
        let site = self.buf.pos();
        self.buf.emit(&crate::ENTRY_PATCH_PAD);
        site
    }

    /// Before a run-time constant-pool helper call at `pc` (the `ldc` String
    /// / Class arms, a deferred `new` / `anewarray`, `multianewarray`), with
    /// the instruction's operands still on the simulated stack: a patchable
    /// exit site whose `OsrExit` map is at `pc` itself, so the interpreter
    /// RE-EXECUTES the instruction (interpreter round i1 wave 44, lane L2;
    /// `interpreter-L2-proposal-exits-at-a-withdrawn-bodys-constant-pool-helper-calls-RETIRED-20261015`,
    /// behind [`CP_HELPER_EXITS_ENABLED`]).
    ///
    /// Everything but the map's position is the post-call site's: the NOP,
    /// the 16-byte stub, the register-preserving verdict tail
    /// (`POST_CALL_EXIT_VERDICT_ONLY`: asked, never parked), the cap, and the
    /// force pass that rewrites the NOP when the body is withdrawn
    /// (`JitCache::force_withdrawn_exit_polls`, which reads
    /// `CompiledMethod::post_call_exit_sites`). So nothing runs until the
    /// body is withdrawn, and a withdrawn body leaves here BEFORE the helper
    /// runs: nothing of the instruction happened yet (no class resolved or
    /// initialised, nothing allocated), and the interpreter runs it once, on
    /// the frame it moved onto the class's current body, against that body's
    /// pool. A body a redefinition did not withdraw keeps the helper and its
    /// translation (`vm::jit::helpers::CpSite`).
    ///
    /// Refused, with nothing emitted or filed: whatever
    /// [`Self::post_call_exit_site_may_be_emitted`] refuses; `pc` a branch
    /// target (a loop header files its own map there, and a map this site
    /// filed first would become the header's for `mode_exit_target`); and
    /// whatever [`Self::branch_mode_exit_target`] refuses for a map at `pc`
    /// with the whole current stack as its operands (a point already at `pc`,
    /// a loop rewrite, no deopt register region, a frame the transfer would
    /// not take, an elided monitor or a self-lock). The instruction's own
    /// points at `pc` come after: the allocation guard's precise frame is a
    /// `RETHROW` point, which neither by-bci reason lookup confuses with this
    /// `REEXECUTE` one (`osr_exit::deopt_reason_at_bci`,
    /// `exceptional_reason_at_bci`), and it is filed in its own by-bci map.
    pub(in crate::x64) fn emit_cp_helper_exit_site(&mut self, pc: usize, is_branch_target: bool) {
        if !CP_HELPER_EXITS_ENABLED
            || !POST_CALL_EXITS_ENABLED
            || is_branch_target
            || !self.post_call_exit_site_may_be_emitted()
        {
            return;
        }
        // The map describes the frame as the NOP sees it, as the conditional
        // back edge's does after its arm's flush: every operand-stack entry
        // in its frame word. Every helper arm this precedes flushes before
        // its call anyway (`emit_pre_safepoint_spill`, or the arm's own
        // flush), so this moves no work onto a path that had none.
        self.flush_scratch_registers();
        let operands = self.stack.len();
        let Some(exit_bci) = self.branch_mode_exit_target(pc, operands) else {
            return;
        };
        self.emit_post_call_exit_nop(exit_bci);
        self.cp_helper_exit_sites = self.cp_helper_exit_sites.saturating_add(1);
    }

    /// Does this compile keep a dispatch record its artifact will own
    /// (`CompiledMethod::_jit_invoke_infos` from `invoke_info`,
    /// `_jit_mic_slots` / `_jit_pic_slots` from `mic_slots` / `pic_slots`),
    /// so that the VM reads the body as one that calls something
    /// (`jit_bridge::wrapped_body_attempt_never_reruns`)? See
    /// [`METHOD_ENTRY_FIRST_MAP_POST_CALL_EXITS_ENABLED`].
    pub(in crate::x64::op_invoke) fn keeps_a_call_record(&self) -> bool {
        !self.invoke_info.is_empty()
            || !self.mic_slots.is_empty()
            || !self.pic_slots.is_empty()
            // 2026-10-07 (item 1 of the i24-L6 page): a call the door bound
            // directly, a cycle cell, or a splice's own call target keeps a
            // record in the artifact too, which these tables miss
            // (`BackendRequest::artifact_keeps_a_call_record`).
            || self.artifact_keeps_a_call_record
    }

    /// Lay down the stubs of every post-call exit site the walk emitted,
    /// after the walk and before `emit_deopt_stubs` (which resolves each
    /// stub's `JNZ` to its successor's reason-7 stub), and record `(site,
    /// stub)` for `CompiledMethod::post_call_exit_sites`. Nothing when no
    /// site was emitted.
    ///
    /// Per site, 16 bytes:
    ///
    /// ```text
    /// stub:  CALL tail           ; E8 rel32 -- the verdict, in the flags
    ///        JNZ  <reason-7 stub of the successor's OsrExit map>
    ///        JMP  <site + 5>     ; stay: back past the NOP
    /// ```
    ///
    /// and once per compile the tail ([`Self::emit_post_call_exit_verdict_tail`]).
    pub(in crate::x64) fn emit_post_call_exit_stubs(&mut self) {
        self.resolve_post_call_exit_claims();
        if !self.post_call_header_claim_census.is_empty() {
            self.report_post_call_header_claim_census();
        }
        let pending = std::mem::take(&mut self.post_call_exits_pending);
        let splice_sites = std::mem::take(&mut self.splice_post_call_exits);
        if (pending.is_empty() && splice_sites.is_empty()) || self.failed {
            return;
        }
        let tail = self.buf.pos();
        self.emit_post_call_exit_verdict_tail();
        // Wave 59 (lane L2): the sites after a real call inside a splice. The
        // same 16-byte stub; its `JNZ` goes to a tag-14 stub that bakes the
        // site's own chain point, found by index (the point has no bci in
        // this compile's key space).
        for exit in splice_sites {
            let stub = self.buf.pos();
            self.buf.emit_byte(0xE8); // CALL rel32 -> the tail
            let call_patch = self.buf.pos();
            self.buf.emit(&[0u8; 4]);
            let call_rel = (tail as i32) - (call_patch as i32 + 4); // Cast: x86-64 rel32 displacement
            self.buf.try_patch_i32(call_patch, call_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
            let leave = self.emit_jcc_rel32_patch(0x85);
            let index = self.splice_post_call_box_ptrs.len();
            self.splice_post_call_box_ptrs.push(exit.point);
            self.deopt_stubs
                .push((leave, index, SPLICE_POST_CALL_STUB_TAG));
            let back = self.emit_jmp_rel32_patch();
            let resume = exit.site + crate::ENTRY_PATCH_PAD.len();
            let back_rel = (resume as i32) - (back as i32 + 4); // Cast: x86-64 rel32 displacement
            self.buf.try_patch_i32(back, back_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
            if let (Ok(site), Ok(stub)) = (u32::try_from(exit.site), u32::try_from(stub)) {
                self.post_call_exit_sites.push((site, stub));
            }
            self.splice_post_call_sources.extend(exit.sources);
        }
        for (site, exit_bci) in pending {
            let stub = self.buf.pos();
            self.buf.emit_byte(0xE8); // CALL rel32 -> the tail
            let call_patch = self.buf.pos();
            self.buf.emit(&[0u8; 4]);
            let call_rel = (tail as i32) - (call_patch as i32 + 4); // Cast: x86-64 rel32 displacement
            self.buf.try_patch_i32(call_patch, call_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                                                               // JNZ -> the successor's reason-7 stub (`emit_deopt_stubs` bakes its
                                                               // `osr_exit_box_ptr_by_bci` map, the one `branch_mode_exit_target`
                                                               // recorded for this site), or, for a claim settled `OwnBeside`
                                                               // (wave 55), the tag-13 stub that bakes the claim's own point.
            let leave = self.emit_jcc_rel32_patch(0x85);
            let tag = if self.post_call_own_box_ptr_by_bci.contains_key(&exit_bci) {
                POST_CALL_OWN_POINT_STUB_TAG
            } else {
                7 // OSR-exit
            };
            self.deopt_stubs.push((leave, exit_bci, tag));
            let back = self.emit_jmp_rel32_patch();
            let resume = site + crate::ENTRY_PATCH_PAD.len();
            let back_rel = (resume as i32) - (back as i32 + 4); // Cast: x86-64 rel32 displacement
            self.buf.try_patch_i32(back, back_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
            if let (Ok(site), Ok(stub)) = (u32::try_from(site), u32::try_from(stub)) {
                self.post_call_exit_sites.push((site, stub));
            }
        }
    }

    /// The shared tail every post-call exit stub `CALL`s: ask the safepoint
    /// slow path for this body's verdict WITHOUT parking
    /// ([`crate::POST_CALL_EXIT_VERDICT_ONLY`]), with every register the
    /// site's state lives in preserved, and return with the flags of
    /// `TEST AL, <the verdict bit the body's polls test>`.
    ///
    /// ```text
    /// tail:  PUSH RAX RCX RDX RBX RSI RDI R8 R9 R10 R11
    ///        MOV  RBX, RSP            ; callee-saved: survives the CALL
    ///        AND  RSP, -16
    ///        SUB  RSP, 0x120          ; home area + 16 XMM slots
    ///        MOVDQU [RSP+0x20+16n], XMMn     (n = 0..15)
    ///        MOV  ARG0, <safepoint flag>     ; names the VM
    ///        MOV  ARG1, <compile id> | POST_CALL_EXIT_VERDICT_ONLY
    ///        MOV  RAX, <safepoint slow path> ; CALL RAX
    ///        MOVDQU XMMn, [RSP+0x20+16n]
    ///        MOV  RSP, RBX
    ///        TEST AL, <bit>
    ///        POP  R11 R10 R9 R8 RDI RSI RBX RDX RCX RAX   ; POP keeps the flags
    ///        RET                                         ; so does RET
    /// ```
    ///
    /// Why no spill and no oop map: the helper does not park in this mode, so
    /// this thread never stops for a collection with the frame's references
    /// only in registers or in the tail's save area. A stop-the-world request
    /// meanwhile finds it either in Rust (the take-over resumes such a thread
    /// and waits for it to arrive cooperatively: at the body's next poll, or
    /// through the exit) or back in this code, where the take-over freezes it
    /// and scans its registers and stack conservatively, and that collection
    /// does not move (`vm/src/jit/xt_root_scan.rs`) -- exactly as for a thread
    /// between two polls of any compiled loop. The site's state is every GPR
    /// and XMM as the call's lowering left them
    /// (RBP, R12-R15 are callee-saved in both ABIs and the helper keeps them;
    /// the flags are dead between two bytecodes). Nothing below RSP is live
    /// there: the call that just returned owned it.
    pub(in crate::x64::op_invoke) fn emit_post_call_exit_verdict_tail(&mut self) {
        // PUSH RAX, RCX, RDX, RBX, RSI, RDI, then R8-R11.
        self.buf.emit(&[0x50, 0x51, 0x52, 0x53, 0x56, 0x57]);
        self.buf
            .emit(&[0x41, 0x50, 0x41, 0x51, 0x41, 0x52, 0x41, 0x53]);
        self.buf.emit(&[0x48, 0x89, 0xE3]); // MOV RBX, RSP
        self.buf.emit(&[0x48, 0x83, 0xE4, 0xF0]); // AND RSP, -16
        self.buf.emit(&[0x48, 0x81, 0xEC]); // SUB RSP, imm32
        self.buf.emit(&POST_CALL_EXIT_TAIL_FRAME.to_le_bytes());
        for xmm in 0u8..16 {
            self.emit_post_call_exit_xmm(0x7F, xmm); // MOVDQU [RSP+..], XMMn
        }
        // Cast: the flag byte's address, as the poll passes it.
        self.emit_mov_imm64(ARG_REGS[0], self.helpers.safepoint_flag_addr as i64);
        self.emit_mov_imm64(
            ARG_REGS[1],
            i64::from(self.compile_id.id()) | crate::POST_CALL_EXIT_VERDICT_ONLY,
        );
        cratonvm_jit_api::assert_helper_call_shape!(
            "safepoint_slow_path",
            int_args = 2,
            returns_value = true
        );
        // Cast: helper address, baked as an immediate.
        self.emit_mov_imm64(RAX, self.helpers.safepoint_slow_path as i64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        for xmm in 0u8..16 {
            self.emit_post_call_exit_xmm(0x6F, xmm); // MOVDQU XMMn, [RSP+..]
        }
        self.buf.emit(&[0x48, 0x89, 0xDC]); // MOV RSP, RBX
                                            // The bit the body's own exit polls test (`emit_safepoint_poll_leaving_to`).
        let bit = if self.mode_exit_polls {
            crate::SAFEPOINT_VERDICT_INNERMOST_FRAME
        } else {
            crate::SAFEPOINT_VERDICT_POLLING_BODY
        };
        // Cast: the verdict bit is below 0x100 (x86-64 imm8).
        self.emit_test_r8_imm8(RAX, bit as u8);
        // POP R11-R8, then RDI, RSI, RBX, RDX, RCX, RAX.
        self.buf
            .emit(&[0x41, 0x5B, 0x41, 0x5A, 0x41, 0x59, 0x41, 0x58]);
        self.buf.emit(&[0x5F, 0x5E, 0x5B, 0x5A, 0x59, 0x58]);
        self.buf.emit_byte(0xC3); // RET
    }

    /// `MOVDQU` between XMM register `xmm` and its save slot
    /// `[RSP + 0x20 + 16 * xmm]` of the verdict tail: `opcode` `0x7F` stores,
    /// `0x6F` loads (`F3 [REX.R] 0F op ModRM(mod=10, reg, rm=SIB) 24 disp32`).
    pub(in crate::x64::op_invoke) fn emit_post_call_exit_xmm(&mut self, opcode: u8, xmm: u8) {
        self.buf.emit_byte(0xF3);
        if xmm >= 8 {
            self.buf.emit_byte(0x44); // REX.R
        }
        self.buf
            .emit(&[0x0F, opcode, 0x84 | ((xmm & 7) << 3), 0x24]);
        let disp: i32 = 0x20 + 16 * i32::from(xmm);
        self.buf.emit(&disp.to_le_bytes());
    }
}

// ---------------------------------------------------------------------------
// Round 15 wave 1 (lane calls, proposal C14W3-5 of `jit-r14-calls3-proposals-RETIRED-20260930.md`):
// the direct-call sentinel path out of line.
// ---------------------------------------------------------------------------
//
// A baked direct `invokestatic` used to end in
//
//     CALL callee ; CMP RAX,1 ; JNO .keep ; <callee-deopt service check> ;
//     <exception check> ; .keep:
//
// so every call that returned normally (all of them, in practice) took a
// branch over ~50 bytes of cold code. With the switch on it ends in
//
//     CALL callee ; CMP RAX,1 ; JO .tail ; .keep:
//
// and `.tail` (emitted after the walk, before the exception-check and deopt
// stubs, by `emit_sp_call_cold_tails`) holds the same two checks, emitted by the
// same functions, followed by `JMP .keep`. The machine state on every edge is
// what it was: the tail runs on exactly the edge the inline checks ran on, in
// the same order, and the checks read nothing the walk changes between the site
// and the end of the method except `dbg_last_pc` / `cur_bc_pc`, which the tail
// record carries and restores.
//
// What the admission (`sp_call_cold_tail_admitted`) keeps out, and why:
//  * a site inside a natively unrolled loop: the unroller copies the loop's
//    machine code and replicates every late-patch list it knows, and this one
//    is not among them (a copy's `JO` would land on the original's tail, which
//    jumps back into the ORIGINAL body);
//  * a compile of rewritten bytecode (`bci_provenance`): same duplication, done
//    on bytecode;
//  * a protected pc: the exception check there publishes a reason-9 frame or
//    takes a local-handler stub, both of which snapshot the walk's state AT the
//    site (`build_and_record_deopt_point`), which the end of the method no
//    longer has;
//  * a site with an inline plan or inside a splice: a splice attempt that rolls
//    back rewinds the buffer, and this list is in no rollback checkpoint;
//  * a caller-held synchronized site (the release sits on the cold edge).
// Anything refused keeps the inline form, byte for byte.

/// `CRATONVM_JIT_SP_CALL_COLD_TAILS` (default OFF, opt-in; round 15 wave 1,
/// lane calls, C14W3-5): a single-pass direct `invokestatic` puts its
/// callee-deopt service check and exception check out of line. Off, the site is
/// emitted exactly as before. Read per site compile, so it adds no process
/// global. Default off until `bench14.sh` prices it (a taken branch per call
/// against the tail's extra `JMP` on the rare sentinel edge).
pub(in crate::x64::op_invoke) fn sp_call_cold_tails_enabled() -> bool {
    false
}

/// `Jcc` condition byte for "RAX was the sentinel" after
/// [`Compiler::emit_cmp_rax_sentinel`] (`JO`).
pub(in crate::x64::op_invoke) const SP_COLD_TAIL_JO: u8 = 0x80;

/// Which callee-deopt service the out-of-line tail emits
/// (`emit_inline_callee_deopt_check` / `_owned_args`, or none).
#[derive(Clone, Copy)]
pub(in crate::x64) enum SpCallColdService {
    Args {
        info: *const crate::JitInvokeInfo,
        n: usize,
        args_base: i32,
    },
    OwnedArgs {
        info: *const crate::JitInvokeInfo,
    },
    Unserviced,
}

/// One deferred cold tail: the hot path's `JO` rel32, where the hot path
/// resumes, and what the tail must emit on the way back.
pub(in crate::x64) struct SpCallColdTail {
    pub(in crate::x64::op_invoke) jo_patch: usize,
    pub(in crate::x64::op_invoke) keep: usize,
    pub(in crate::x64::op_invoke) service: SpCallColdService,
    pub(in crate::x64::op_invoke) ret_type: u8,
    pub(in crate::x64::op_invoke) throw_bci: usize,
    pub(in crate::x64::op_invoke) bc_pc: usize,
}

impl Compiler {
    /// May the direct call at `pc` take the out-of-line cold tail? See the
    /// section comment for each term.
    pub(in crate::x64::op_invoke) fn sp_call_cold_tail_admitted(&self, pc: usize) -> bool {
        sp_call_cold_tails_enabled()
            && merged_call_sentinel_enabled()
            && self.bci_provenance.is_none()
            && self.inline_scope_stack.is_empty()
            && !self.inline_sites.contains_key(&pc)
            && !self.pc_is_protected(pc)
            && !self.pc_is_protected(self.dbg_last_pc)
            && !self
                .unroll_loops
                .iter()
                .any(|&(a, b, _)| a.min(b) <= pc && pc <= a.max(b))
    }

    /// The hot half at the site: `CMP RAX,1 ; JO .tail`, and the record the
    /// tail is emitted from. The fall-through is `.keep`, a join (the tail
    /// jumps back to it), so the reload-elision mirror is dropped here as
    /// `patch_rel32_to_here` would. Counted as a post-invoke check at the site,
    /// where the walk reads the count (`emit_post_call_exit_site`).
    pub(in crate::x64::op_invoke) fn emit_sp_call_cold_tail_branch(&mut self, service: SpCallColdService, ret_type: u8) {
        self.emit_cmp_rax_sentinel();
        let jo_patch = self.emit_jcc_rel32_patch(SP_COLD_TAIL_JO);
        self.slot_mirror = None;
        self.post_invoke_checks_emitted = self.post_invoke_checks_emitted.wrapping_add(1);
        self.sp_call_cold_tails.push(SpCallColdTail {
            jo_patch,
            keep: self.buf.pos(),
            service,
            ret_type,
            throw_bci: self.dbg_last_pc,
            bc_pc: self.cur_bc_pc,
        });
    }

    /// Emit every deferred cold tail. Called by `compile_bytecode` after the
    /// walk and BEFORE `emit_local_handler_stubs` / `emit_exception_check_stub`
    /// / `emit_deopt_stubs`, which consume the edges the tails' exception
    /// checks record.
    pub(in crate::x64) fn emit_sp_call_cold_tails(&mut self) {
        if self.sp_call_cold_tails.is_empty() {
            return;
        }
        let tails = std::mem::take(&mut self.sp_call_cold_tails);
        let (last_pc, bc_pc, checks) = (
            self.dbg_last_pc,
            self.cur_bc_pc,
            self.post_invoke_checks_emitted,
        );
        for tail in tails {
            // Also drops the reload-elision mirror: the tail is a join.
            self.patch_rel32_to_here(tail.jo_patch);
            self.dbg_last_pc = tail.throw_bci;
            self.cur_bc_pc = tail.bc_pc;
            match tail.service {
                SpCallColdService::Args { info, n, args_base } => {
                    self.emit_inline_callee_deopt_check(info, n, args_base)
                }
                SpCallColdService::OwnedArgs { info } => {
                    self.emit_inline_callee_deopt_check_owned_args(info)
                }
                SpCallColdService::Unserviced => {}
            }
            self.emit_post_invoke_exception_check(tail.ret_type);
            // JMP rel32 back to `.keep`.
            let back = self.emit_jmp_rel32_patch();
            let rel = (tail.keep as i32) - (back as i32 + 4); // Cast: x86-64 rel32 displacement
            self.buf.try_patch_i32(back, rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
        }
        self.dbg_last_pc = last_pc;
        self.cur_bc_pc = bc_pc;
        self.post_invoke_checks_emitted = checks;
        self.slot_mirror = None;
    }
}

impl Compiler {
    /// JIT round 16 (lane throwpath, TP-2): how does the String access
    /// intrinsic at `pc` (`length`, `isEmpty`, `charAt`, `hashCode`) treat a
    /// null receiver?
    ///
    /// The deopt (stub reason 6, `ReceiverTypeChanged`) handed the rest of the
    /// activation to the interpreter at every null receiver, and its
    /// `RecompileAndReinterpret` produced the same arm again: in
    /// `R16ThrowpathPlain` `npe-invoke` every one of the 60 003 caught NPEs of
    /// `vNullStr.length()` was raised by the interpreter after the first.
    /// With `CRATONVM_JIT_STRING_INTRINSIC_NPE=1` the NPE is raised in
    /// compiled code instead, as the array opcodes raise theirs:
    ///
    /// * [`StringNullReceiver::SharedStub`]: the shared null-check stub
    ///   (`jit_npe_with_action` with the site's trap key, then the implicit
    ///   local-handler probe).
    /// * [`StringNullReceiver::PreciseFrame`] (JIT round 16 wave 12, lane
    ///   throw3): inside a protected range of a method compiled with precise
    ///   exceptional frames, the reason-10 stub the array opcodes take there
    ///   (`emit_precise_array_npe_check`): the frame at this bci is published,
    ///   so a handler that reads a local the range wrote sees its value.
    ///   Wave 11 found the switch measured "no gain" because it never engaged
    ///   where it was measured: `npeInvoke`'s handler reads `c`, written in
    ///   the range, so the method compiles with precise frames and the site
    ///   is protected -- the old rule kept the deopt there (code length 1466
    ///   bytes in both arms, and `CRATONVM_DBG_DEOPT=1` showed the same
    ///   `ReceiverTypeChanged` deopts with the switch on, escalating to
    ///   `MakeNotCompilable`).
    ///
    /// Not inside a splice (the stubs' local-handler probe range-tests the bci
    /// against THIS method's exception table, and the precise stub publishes
    /// THIS method's frame): that keeps the deopt. Not memoised
    /// (`process_global_statics_ratchet`): read per String intrinsic site of a
    /// compile.
    pub(in crate::x64) fn string_intrinsic_null_receiver_raises_npe(
        &self,
        pc: usize,
    ) -> StringNullReceiver {
        string_null_receiver_arm(
            true,
            !self.inline_scope_stack.is_empty(),
            self.precise_exception_frames && self.pc_is_protected(pc),
        )
    }
}

/// [`Compiler::string_intrinsic_null_receiver_raises_npe`]'s rule on its
/// inputs: the switch, a splice, and "a protected site of a method compiled
/// with precise exceptional frames".
pub(in crate::x64::op_invoke) fn string_null_receiver_arm(
    enabled: bool,
    in_splice: bool,
    precise_protected: bool,
) -> StringNullReceiver {
    if !enabled || in_splice {
        StringNullReceiver::Deopt
    } else if precise_protected {
        StringNullReceiver::PreciseFrame
    } else {
        StringNullReceiver::SharedStub
    }
}

/// [`Compiler::string_intrinsic_null_receiver_raises_npe`]'s answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::x64) enum StringNullReceiver {
    /// The reason-6 guard deopt (the default).
    Deopt,
    /// The shared null-check stub: the NPE in compiled code, frame-less.
    SharedStub,
    /// The reason-10 precise-frame NPE stub (a protected site of a method
    /// compiled with precise exceptional frames).
    PreciseFrame,
}







/// `CRATONVM_JIT_SB_GROW` (strings round 2, `str17`; default ON, `=0`
/// restores the decline edge): the `StringBuilder` append intrinsics grow a
/// full LATIN1 payload through `DirectHelperTable::sb_grow` and run their body
/// again (`emit_sb_grow_stub`), instead of declining into the generic
/// dispatcher and the JDK's compiled growth chain -- ~2 400 cycles a growth,
/// on 29% of `R16Bench strings`' iterations. Read once per append intrinsic
/// site of a compile.
pub(in crate::x64::op_invoke) fn sb_grow_enabled() -> bool {
    true
}

/// What the inline `StringBuilder.toString()` body bakes at compile time
/// (JIT round 17 wave 1, lane strings, STR16-P1): the `java/lang/String`
/// compact recipe it allocates against, or the reason it emits only a decline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::x64) struct SbToStringPlan {
    /// `java/lang/String`'s header class id.
    pub(in crate::x64) string_class_id: u32,
    /// The compact String's TOTAL size (8-byte header included), a multiple
    /// of 8.
    pub(in crate::x64) string_total: i32,
    /// `String.value`'s compact displacement: a bare 8-byte pointer.
    pub(in crate::x64) value_off: i32,
    /// Address and compile-time value of String's layout-replace counter.
    pub(in crate::x64) replace_guard: (usize, u32),
    /// The largest `count` served inline: both objects stay under the inline
    /// array cap the allocator itself honours.
    pub(in crate::x64) max_count: i32,
    /// `ARRAY_LENGTH_OFFSET` / `ARRAY_DATA_OFFSET` / `KIND_TAGS_BYTE_OFFSET`
    /// as disp8 bytes (all screened to `0..=127`).
    pub(in crate::x64) len_d8: u8,
    pub(in crate::x64) data_d8: u8,
    pub(in crate::x64::op_invoke) kind_d8: u8,
    /// The `KIND_TAGS_BYTE_OFFSET` byte of every `byte[]` (bits 6..7 are
    /// reserved zero on arrays; `primitive_array_kind_tags_byte("[B")`).
    pub(in crate::x64::op_invoke) byte_array_tag: u8,
    /// The mark word of a fresh `byte[]` (`ObjectHeader::new`'s own bits).
    pub(in crate::x64) byte_array_mark: i32,
    /// `String.coder`'s compact `(offset, is_byte)`, for the UTF16 arm
    /// (round 17 wave 2, lane sbzgc, `CRATONVM_JIT_SB_TOSTRING_UTF16`), which
    /// stores `coder = UTF16` into the fresh String; `None` refuses that arm
    /// only. A 1- or 4-byte slot inside the String's body, at the layout's own
    /// offset, with the `+1` high-byte probe of the scan still a disp8.
    pub(in crate::x64) utf16_coder: Option<(i32, bool)>,
}

/// Which collector obligations the inline `toString()` discharges at a site
/// (JIT round 17 wave 2, lane sbzgc), chosen at compile time by
/// [`sb_to_string_gc_arm`].
///
/// Both extra checks run BEFORE the first store and decline into the ordinary
/// call, so the fast path stays call-free on every backend (no safepoint map,
/// no OOM routing, no spill discipline), and both are proofs that the
/// collector's own barrier for the single reference store, `String.value =
/// the byte[]`, would have done nothing:
///
/// * Generational: a fresh young receiver needs no card, a null old value no
///   SATB entry (the wave-1 premise; no run-time check).
/// * ZGC (`zgc_start_bits`): both objects are announced with the inline
///   start-bit store -- BEFORE the commit since nio21 wave 10 L1
///   (`CRATONVM_ZGC_JIT_START_BIT_BEFORE_COMMIT`; after it with `=0`),
///   exactly as `emit_inline_tlab_new` /
///   `emit_inline_tlab_newarray` announce one, with every condition of the
///   table's contract checked first. `blocked == 0` is one of them, and it
///   means no mark is running (`ZgcRealHeap::set_field` publishes an SATB
///   entry only under `mark_active`, and the old value is the TLAB's zero
///   anyway) and the heap is not generational (`note_ref_store` cards only
///   with `has_old_objects`). No colour bits: this ZGC never arms its load
///   barrier (`docs/GC.md`, backend table).
/// * G1 (`g1_same_region`): `post_write_barrier_rset` returns without a card
///   or a remembered-set edge for a same-region store, and the SATB
///   pre-barrier logs nothing for a null old value; the run-time check is
///   that same-region test, `((S - base) ^ (A - base)) & region_mask == 0`,
///   against the published `JIT_G1_BARRIER` table. Objects above TAMS are
///   implicitly live, as for every inline `new` on G1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::x64) struct SbToStringGc {
    /// ZGC: the call-free start-bit pre-check and both start bits.
    pub(in crate::x64) zgc_start_bits: bool,
    /// A live G1 table: the same-region pre-check.
    pub(in crate::x64) g1_same_region: bool,
}

/// What one emitted copy of [`Compiler::emit_sb_to_string_tail`] builds (JIT
/// round 17 wave 6, lane sbzgc2): the String's coder, whether the copy
/// narrows a UTF16 payload to LATIN1, and which
/// [`crate::metrics::SB_TO_STRING_CENSUS`] row counts it when counting is on.
#[derive(Clone, Copy, Debug)]
pub(in crate::x64) struct SbToStringArm {
    /// `Some` stores `coder = UTF16` at that `(offset, is_byte)`; `None`
    /// leaves the zeroed LATIN1.
    pub(in crate::x64) utf16_coder: Option<(i32, bool)>,
    /// ECX chars of the UTF16 source in RDX copied as their low bytes (the
    /// compress, `CRATONVM_JIT_SB_TOSTRING_COMPRESS`), rather than ECX bytes.
    pub(in crate::x64) narrow: bool,
    /// The run-time `taken-*` row.
    pub(in crate::x64) taken_row: usize,
}

/// The compile-time facts [`sb_to_string_gc_arm`] decides from, gathered by
/// the emitter (and spelled out by the unit tests).
#[derive(Clone, Copy, Debug)]
pub(in crate::x64) struct SbToStringGcInputs {
    /// `jit_tlab_registration_required()`: ZGC.
    pub(in crate::x64) zgc_registration: bool,
    /// `CRATONVM_ZGC_JIT_INLINE_ANNOUNCE` (the collector publishes its table).
    pub(in crate::x64) zgc_start_bit_requested: bool,
    /// `CRATONVM_JIT_IR_ZGC_INLINE_ARRAY_ANNOUNCE`, the inline array lever.
    pub(in crate::x64) zgc_array_announce: bool,
    /// `g1_barrier_table_live`: a G1 collector published its geometry.
    pub(in crate::x64) g1_table_live: bool,
    /// `region_bounds_are_live`: the Generational backend's bounds.
    pub(in crate::x64) generational_bounds_live: bool,
}

/// The collector name the `CRATONVM_DBG_JITC` line carries.
pub(in crate::x64) fn sb_to_string_gc_name(i: &SbToStringGcInputs) -> &'static str {
    if i.zgc_registration {
        "ZGC"
    } else if i.g1_table_live {
        "G1"
    } else if i.generational_bounds_live {
        "Generational"
    } else {
        "unknown"
    }
}

/// The per-collector arm of the inline `toString()`, or why there is none.
///
/// A ZGC site needs the inline start-bit table (without it every pre-check
/// would decline) and honours the array announce lever, since this is an
/// inline `newarray`. A live G1 table adds the same-region check on whatever
/// backend (two VMs in one process: the check is sound everywhere). A site
/// with none of the three premises declines.
pub(in crate::x64) fn sb_to_string_gc_arm(i: SbToStringGcInputs) -> Result<SbToStringGc, &'static str> {
    if i.zgc_registration {
        if !i.zgc_start_bit_requested {
            return Err("ZGC inline start-bit announce off (CRATONVM_ZGC_JIT_INLINE_ANNOUNCE=0)");
        }
        if !i.zgc_array_announce {
            return Err(
                "ZGC inline array announce off (CRATONVM_JIT_IR_ZGC_INLINE_ARRAY_ANNOUNCE=0)",
            );
        }
    } else if !i.g1_table_live && !i.generational_bounds_live {
        return Err("no collector premise (no ZGC table, G1 table or generational bounds)");
    }
    Ok(SbToStringGc {
        zgc_start_bits: i.zgc_registration,
        g1_same_region: i.g1_table_live,
    })
}

/// The compile-time half of the inline `toString()`: every screen that does
/// not depend on the receiver. `Err` names the refusal; the site then emits
/// one unconditional decline (the ordinary call) and nothing else.
///
/// Screens, each a reason the fast path could not keep its contract (the
/// collector is [`sb_to_string_gc_arm`]'s question, round 17 wave 2):
/// * the inline-TLAB levers (`CRATONVM_NO_JIT_INLINE_TLAB_NEW[ARRAY]`, zero
///   elision off), because this is an inline `new` and an inline `newarray`;
/// * narrow oops (the `value` store is a bare 8-byte pointer);
/// * no compact `String` recipe for exactly its registered field count in a
///   single layout domain (`compact_tlab_total_size`), or a `value` cell that
///   is not an 8-byte slot at the layout's own offset.
pub(in crate::x64::op_invoke) fn sb_to_string_plan(outer: &crate::StringFieldLayout) -> Result<SbToStringPlan, &'static str> {
    if !crate::jit_sb_to_string_inline_enabled() {
        return Err("switch off");
    }
    string_alloc_plan(outer)
}

/// [`sb_to_string_plan`] without its switch: the compact `String` + exact
/// `byte[]` recipe one inline TLAB bump builds. Shared by the inline
/// `StringBuilder.toString()` and (JIT round 19 proposal wave 4, lane
/// strgap4, STRGAP19-3) the inline concat, which has its own switch.
pub(in crate::x64) fn string_alloc_plan(
    outer: &crate::StringFieldLayout,
) -> Result<SbToStringPlan, &'static str> {
    use cratonvm_types::{ArrayElementType, ClassId, ObjectHeader, ObjectKind};
    if !inline_tlab_new_enabled()
        || !inline_tlab_newarray_enabled()
        || !inline_tlab_zero_elision_enabled()
    {
        return Err("inline TLAB lever off");
    }
    if narrow_oops_enabled() || outer.value_compact_is_narrow {
        return Err("narrow oops");
    }
    let cid = outer.string_class_id;
    if cid == 0 {
        return Err("no String class id");
    }
    // The replace count BEFORE the recipe is read: a replacement between the
    // two reads fails the run-time guard, never the other way round.
    let (guard_addr, guard_value) = cratonvm_types::layout_replace_guard(cid);
    let fields = cratonvm_types::compact_field_count(cid).ok_or("no compact String layout")?;
    let total = cratonvm_types::compact_tlab_total_size(cid, fields as usize)
        .ok_or("no compact String recipe")?;
    let value_off = match cratonvm_types::compact_field_disp_storage(cid, outer.value_field_index) {
        Some((off, storage)) if storage.size_runtime() == 8 => off,
        _ => return Err("String.value is not an 8-byte compact slot"),
    };
    if total % 8 != 0
        || total > 256
        || value_off < cratonvm_types::COMPACT_HEADER_SIZE
        || value_off.saturating_add(8) > total
        || i32::try_from(value_off).ok() != Some(outer.value_compact_offset)
    {
        return Err("String recipe shape");
    }
    let d8 = |v: usize| u8::try_from(v).ok().filter(|b| *b <= 127);
    let (Some(len_d8), Some(data_d8), Some(kind_d8)) = (
        d8(cratonvm_types::ARRAY_LENGTH_OFFSET),
        d8(cratonvm_types::ARRAY_DATA_OFFSET),
        d8(cratonvm_types::KIND_TAGS_BYTE_OFFSET),
    ) else {
        return Err("header offset past disp8");
    };
    // The String is placed at `array + DATA + align8(count)`: an 8-aligned
    // array header and an 8-aligned data offset keep it on the 8-byte grid
    // every walker (and ZGC's start bitmap) assumes. True today (16); a
    // future header that broke it costs coverage, not a misaligned object.
    if data_d8 % 8 != 0 {
        return Err("array data offset not 8-aligned");
    }
    // The UTF16 arm's `coder` store (round 17 wave 2, lane sbzgc).
    let utf16_coder = if outer.has_coder && data_d8 < 127 {
        match cratonvm_types::compact_field_disp_storage(cid, outer.coder_field_index) {
            Some((off, storage)) => {
                let width = storage.size_runtime();
                let shape_ok = (width == 1 || width == 4)
                    && (width == 1) == outer.coder_compact_is_byte
                    && off >= cratonvm_types::COMPACT_HEADER_SIZE
                    && off.saturating_add(width as usize) <= total
                    && i32::try_from(off).ok() == Some(outer.coder_compact_offset);
                if shape_ok {
                    i32::try_from(off).ok().map(|o| (o, width == 1))
                } else {
                    None
                }
            }
            None => None,
        }
    } else {
        None
    };
    let byte_array_tag =
        cratonvm_types::primitive_array_kind_tags_byte("[B").ok_or("no byte[] kind tag")?;
    let byte_array_mark = ObjectHeader::new(
        ClassId::new(0),
        ObjectKind::Array,
        ArrayElementType::Byte,
        0,
        0,
    )
    .mark_word
    .load(std::sync::atomic::Ordering::Relaxed);
    // Both objects, the 8-byte alignment slop and the array's round-up stay
    // under the cap `emit_inline_tlab_newarray` honours for one array.
    let fixed = usize::from(data_d8) + total + 16;
    let max_count = Compiler::INLINE_ARRAY_TLAB_MAX_ALLOC
        .checked_sub(fixed)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or("cap")?;
    Ok(SbToStringPlan {
        string_class_id: cid,
        string_total: i32::try_from(total).map_err(|_| "String recipe shape")?,
        value_off: i32::try_from(value_off).map_err(|_| "String recipe shape")?,
        replace_guard: (guard_addr as usize, guard_value),
        max_count,
        len_d8,
        data_d8,
        kind_d8,
        byte_array_tag,
        // Cast: the mark word's bits as an imm32.
        byte_array_mark: byte_array_mark as i32,
        utf16_coder,
    })
}

/// `CRATONVM_STRING_LITERAL_IDENTITY` (JIT round 17 wave 4, lane sbempty;
/// default ON, `=0` kill): the inline `toString()` declines an empty LATIN1
/// builder into the call, which answers JDK 25's `""` literal. Read at compile
/// time, once per emitted site, like the other levers of this body; the same
/// name gates the natives' literal answers (`native-builtins` `lang_string`
/// `string_literal_identity_enabled`).
pub(in crate::x64::op_invoke) fn empty_builder_declines_to_call() -> bool {
    true
}

