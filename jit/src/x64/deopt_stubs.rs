// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Deoptimisation points, exception checks and the out-of-line stub block.
//!
//! Two related things live here. `build_and_record_deopt_point` and its
//! callers record what the interpreter frame would have to look like at a
//! given bci — locals and operand stack as `FrameValue`s, reconstructable from
//! live machine state. `emit_deopt_stubs` and the other stub emitters then lay
//! down the cold code the fast path branches to: bounds-check and null-check
//! failures, the post-call exception check, the allocation OOM check, and the
//! deopt trampolines themselves.
//!
//! The stub block is emitted last, after the whole body, so the fast path
//! stays contiguous; that is also why the bci-baking sites are here rather
//! than at the guards they serve.

use super::*;

impl Compiler {
    /// deopt-osr Step 1: record a precise deopt-exit snapshot (the interpreter
    /// frame state — locals + operand stack as `FrameValue`s — reconstructable
    /// from live machine state) at an eligible guard whose loop-header/canonical
    /// bytecode index is `bci`.
    ///
    /// Provenance comes from the current register allocation (`reg_for_local` /
    /// `xmm_for_local`, else the spilled frame slot) and the positive oop source
    /// (`local_oop_masks`/`local_oop_reached` for locals, `stack_oop_marks` for
    /// the operand stack — `Object`/ref iff the bit is SET *and* reached). The
    /// primitive width/type source is a deferred follow-up; a register-resident
    /// slot is recorded by provenance only and the `can_deopt_resume` gate (a
    /// later step) excludes the ambiguous cases, so nothing resumes on a mistyped
    /// slot.
    ///
    /// EMIT-AND-DISCARD: the point is recorded into `deopt_points` / `deopt_boxes`
    /// (transferred to `CompiledMethod` at finalize) but no live path consumes it
    /// yet, so this does not change the `i64::MIN` re-run behaviour.
    pub(super) fn emit_deopt_snapshot_at_guard(&mut self, bci: usize) {
        let box_ptr =
            self.build_and_record_deopt_point(bci, crate::deopt::DeoptReason::BoundsCheck);
        self.deopt_box_ptr_by_bci.insert(bci, box_ptr);
    }

    /// Step 6: Record a `DeoptimizationPoint` for a call-site guard bail using
    /// the CURRENT stack state. Must be called right after `flush_scratch_registers()`
    /// (so every Scratch/Xmm operand is in a `Frame` slot) and BEFORE any
    /// `pop_stack()` calls for the intrinsic, so the snapshot reflects the JVM's
    /// abstract operand stack at this `bci`. The same `deopt_box_ptr_by_bci` map
    /// is used as for loop-header BCE guards; `emit_deopt_stubs` routes reason-2
    /// and reason-6 bails to the frame-deopt trampoline when a snapshot is found.
    /// Idempotent: a second call for the same `bci` is a no-op.
    /// Only called when `deopt_real_enabled()`; production builds are unaffected.
    /// Index every ordinary invoke site's argument type tags by bci, from the
    /// descriptors already in `invoke_info`. One pass, before the walk.
    ///
    /// `indy_arg_type_tags` produces ONE TAG PER COMPACT SLOT, which is the
    /// same shape as this backend's abstract operand stack (a `long` occupies
    /// one entry, not two) — that correspondence is what makes the tags
    /// index-alignable with `self.stack`, and it is already pinned by
    /// `indy_arg_type_tags_one_tag_per_compact_slot`.
    ///
    /// Receiver-inclusive by construction without special-casing it: the tags
    /// cover only the descriptor's parameters, and they are aligned to the TOP
    /// of the stack, so an instance call's receiver sits below the tagged range
    /// and keeps its oop-mark-derived encoding.
    pub(super) fn index_invoke_arg_types(&mut self) {
        for &(pc, info) in &self.invoke_info {
            if info.is_null() {
                continue;
            }
            // SAFETY: `invoke_info` holds pointers to `JitInvokeInfo` boxes the
            // caller keeps alive for the whole compile (they are also baked
            // into the emitted code as call-site metadata).
            let descriptor = unsafe { (*info).descriptor };
            let tags = crate::indy_arg_type_tags(descriptor);
            if !tags.is_empty() {
                self.invoke_stack_arg_types.insert(pc, tags);
            }
        }
    }

    /// Compute the per-bci operand-stack kinds from the resolved per-pc
    /// metadata. See `x64::stack_kinds` for the analysis and its safety
    /// argument; this is only the adapter that collects its inputs.
    ///
    /// Call arities are gathered from all three dispatch shapes, because the
    /// analysis needs an arity at EVERY call site — one unmodelled call poisons
    /// the rest of the method:
    ///
    ///   * `invoke_info` — the descriptor is present, so the arity is
    ///     `indy_arg_type_tags(descriptor).len()`, which counts one entry per
    ///     compact slot exactly like the abstract stack;
    ///   * `direct_calls` — `num_params` is already compact slots and excludes
    ///     the receiver (the opcode supplies that);
    ///   * `indy_info` — carries `count_param_slots(descriptor)` directly.
    ///
    /// `invoke_info` is applied last so a site with both shapes takes the
    /// descriptor-derived answer.
    pub(super) fn analyze_stack_kinds(&mut self, code: &[u8], code_len: usize) {
        use super::stack_kinds::{analyze, StackKindInputs};

        let field_types: FxHashMap<usize, u8> =
            self.field_info.iter().map(|&(pc, _, tag)| (pc, tag)).collect();
        let static_types: FxHashMap<usize, u8> = self
            .static_field_info
            .iter()
            .map(|&(pc, _, _, tag, _)| (pc, tag))
            .collect();

        let mut calls: FxHashMap<usize, (usize, u8)> = FxHashMap::default();
        for &(pc, args, ret, _, _) in &self.indy_info {
            calls.insert(pc, (args, ret));
        }
        for (pc, dc) in &self.direct_calls {
            calls.insert(*pc, (dc.num_params, dc.return_type));
        }
        for &(pc, info) in &self.invoke_info {
            if info.is_null() {
                continue;
            }
            // SAFETY: `invoke_info` holds pointers to `JitInvokeInfo` boxes the
            // caller keeps alive for the whole compile.
            let (descriptor, ret) = unsafe { ((*info).descriptor, (*info).return_type) };
            calls.insert(pc, (crate::indy_arg_type_tags(descriptor).len(), ret));
        }

        let ldc_refs: FxHashSet<usize> = self
            .ldc_string_info
            .iter()
            .map(|&(pc, _, _)| pc)
            .chain(self.ldc_class_info.iter().map(|&(pc, _, _)| pc))
            .collect();

        // Only pcs the constant-pool resolver actually reduced to an immediate.
        // A site it never saw must stay `Unknown` rather than default to the
        // non-floating-point member of its pair.
        let ldc_resolved: FxHashSet<usize> = self
            .ldc_info
            .iter()
            .map(|&(pc, _)| pc)
            .chain(self.ldc2w_info.iter().map(|&(pc, _)| pc))
            .collect();

        let inputs = StackKindInputs {
            field_types,
            static_types,
            calls,
            ldc_refs: &ldc_refs,
            ldc_fp: &self.ldc_fp_pcs,
            ldc_resolved: &ldc_resolved,
        };
        self.stack_kinds = analyze(code, code_len, &inputs);
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STACK_KINDS").is_some() {
            eprintln!(
                "[stack-kinds] {} answered {} of {code_len} pcs (calls={} fields={} statics={})",
                self.method_key,
                self.stack_kinds.answered(),
                self.indy_info.len() + self.direct_calls.len() + self.invoke_info.len(),
                self.field_info.len(),
                self.static_field_info.len(),
            );
        }
    }

    pub(super) fn snapshot_pre_intrinsic_call(&mut self, bci: usize, reason: crate::deopt::DeoptReason) {
        if self.deopt_box_ptr_by_bci.contains_key(&bci) {
            return;
        }
        let box_ptr = self.build_and_record_deopt_point(bci, reason);
        self.deopt_box_ptr_by_bci.insert(bci, box_ptr);
    }

    /// Build a `DeoptimizationPoint` capturing the interpreter frame at `bci`
    /// from the current regalloc provenance (`reg_for_local`/`xmm_for_local`/
    /// `local_offset` + the simulated operand stack) and the positive oop sources
    /// (`local_oop_masks`/`local_oop_reached`/`stack_oop_marks`), record it in
    /// `deopt_points` + a stable `deopt_boxes` copy, and return the boxed-point
    /// pointer (for the caller to key by bci). Shared by the BCE-guard snapshot
    /// and the OSR-exit map, which differ only in `reason` and which bci→ptr map
    /// they populate. Emits NO machine code — pure metadata.
    /// Phase B (real-frame-deopt x64 backport): build the `VirtualObjectState`
    /// for a scalar-replaced object `new_pc` whose dummy ref is live in a local at
    /// a deopt point. Each field `k`'s value is read from its frame slot
    /// `[rbp - (field_base_offset + k*SLOT_SIZE)]` — always the field's live value
    /// (zero-filled at `new`, overwritten by `putfield`), so reading it is
    /// temporally correct at any PC. The slot's `FrameValue` width/ref-ness is
    /// typed by `sr_field_types`; a field never accessed by the method defaults to
    /// `Int(0)` (sound: a non-escaping object's continuation reads the same
    /// bytecode, so a never-accessed field is provably never read post-resume).
    /// Returns `None` if the object metadata is missing (⇒ caller bails the slot).
    fn sr_virtual_object_state(&self, new_pc: usize) -> Option<crate::deopt::VirtualObjectState> {
        let obj = self.scalar_replaced.get(&new_pc)?;
        let field_values = sr_field_values(obj.num_fields, obj.field_base_offset, |k| {
            self.sr_field_types.get(&(new_pc, k)).copied()
        });
        Some(crate::deopt::VirtualObjectState {
            id: new_pc,
            class_id: obj.class_id,
            num_fields: obj.num_fields,
            field_values,
        })
    }

    pub(super) fn build_and_record_deopt_point(
        &mut self,
        bci: usize,
        reason: crate::deopt::DeoptReason,
    ) -> *const crate::deopt::DeoptimizationPoint {
        use crate::deopt::{DeoptAction, DeoptimizationPoint, FrameState, FrameValue};

        // Cast: buffer position/length to encoding offset (i32/u32)
        let native_offset = self.buf.pos() as u32;

        // ── THE COORDINATE CHANGE ────────────────────────────────────────
        //
        // `bci` is an EMITTER pc. Under a bytecode loop rewrite it indexes the
        // REWRITTEN method, and every use of it below — `sr_local_prov_at`,
        // `local_oop_reached`/`local_oop_masks`, `local_liveness`,
        // `local_kinds_refined`, `indy_stack_arg_types`, `sr_monitor_at` — is
        // an analysis OF that rewritten method, so all of them keep it. So do
        // the callers' `*_box_ptr_by_bci` keys and `emit_deopt_stubs`' stub
        // sharing, which must stay per-copy.
        //
        // `DeoptimizationPoint::bci` and `FrameState::bci` are different: they
        // are the two fields the VM RESUMES AT, range-tests against the
        // method's interpreter exception table, and matches against
        // `osr_pc_to_native`'s (interpreter-space) keys. They — and only they —
        // are published through `orig_bci`, which is the identity whenever
        // nothing was rewritten, so an ordinary compile is unchanged.
        //
        // Resuming a copy at its original bci is exactly right: copy `j` of an
        // unrolled body IS iteration `i + j`, executing the same bytecode with
        // the same abstract frame, so the interpreter continuing at that bci
        // continues the same computation. `compile_with_param_slots` re-derives
        // this translation from `deopt_point_pcs` and discards the method if it
        // does not hold — including for the versioning guard's synthetic bytes,
        // which are an image of no instruction at all.
        // Cast: bytecode index to u32 (non-negative, fits)
        let resume_bci = self.orig_bci(bci) as u32;

        // Phase B: locals holding a live scalar-replaced object at this bci →
        // `local_index → new_pc`. Emitted as `VirtualObject` (first occurrence) /
        // `VirtualObjectRef` (a shared later occurrence) below, so a guard deopt
        // re-materializes the elided object instead of resuming with the dummy
        // null. Empty unless the method scalar-replaced an object live here.
        let sr_here: FxHashMap<usize, usize> = self
            .sr_local_prov_at
            .get(&bci)
            .map(|v| v.iter().copied().collect())
            .unwrap_or_default();
        let mut sr_emitted: std::collections::HashSet<usize> = std::collections::HashSet::new();

        // Locals: oop-ness from the intersection-dataflow mask (only when the
        // forward dataflow reached this PC; otherwise treat as non-oop). A slot
        // beyond bit 63, or in an unmapped method, reads as non-oop here — sound
        // only because `can_deopt_resume` (later) gates such methods off.
        let oop_reached = self.local_oop_reached.get(bci).copied().unwrap_or(false);
        let oop_mask = if oop_reached {
            self.local_oop_masks.get(bci).copied().unwrap_or(0)
        } else {
            0
        };
        let mut locals = Vec::with_capacity(self.num_locals);
        for i in 0..self.num_locals {
            // A dead local must not be decoded from its machine home. JVM
            // frames initialise unused slots with a non-pointer sentinel
            // (`u64::MAX` in the x64 frame); method-wide type classification
            // may still label that slot as a reference because a later handler
            // first defines it with `astore`. Decoding the stale sentinel as
            // `StackSlotRef` produces an invalid ObjectRef before the handler
            // can overwrite it. `Undefined` maps to an inert zero and is sound
            // precisely because liveness proves no path reads the old value
            // before its next definition.
            // Only act on a COMPUTED liveness answer. An uncovered pc (no
            // basic block reaches it) reads as 0 = "nothing live", and acting
            // on that would drop every local in the frame.
            if i < 64 && self.local_liveness_covered.get(bci).copied().unwrap_or(false) {
                let live_here = self.local_liveness.get(bci).copied().unwrap_or(u64::MAX);
                if live_here & (1u64 << i) == 0 {
                    // `CRATONVM_DBG_EXCFRAME=1` reports every local DROPPED
                    // from a snapshot. That is the actionable signal for this
                    // whole bug class: a handler that reads a dropped local
                    // sees 0 / null, silently and without a crash. If a value
                    // you expect at a handler appears here, the liveness at
                    // `bci` is not modelling the exception edge that reaches
                    // it — see `regalloc::handler_live_mask`.
                    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_EXCFRAME").is_some() {
                        eprintln!(
                            "[excframe] DROP local={i} at bci={bci} live_mask={live_here:#x} \
                             precise={} handler_ranges={} method={}",
                            self.precise_exception_frames,
                            self.exception_ranges_dbg_len,
                            self.method_key,
                        );
                    }
                    locals.push(FrameValue::Undefined);
                    continue;
                }
            }
            // Phase B: a local holding a scalar-replaced object's dummy ref is
            // emitted as a `VirtualObject` (first sighting) / `VirtualObjectRef`
            // (a shared later sighting), NOT as a `StackSlotRef`/`RegisterRef` to
            // the zeroed dummy — which would resume with a bogus null. Takes
            // precedence over the oop-mask path below (the slot's machine home
            // holds 0; the object's real state lives in its field slots).
            if let Some(&new_pc) = sr_here.get(&i) {
                if sr_emitted.contains(&new_pc) {
                    locals.push(FrameValue::VirtualObjectRef(new_pc));
                    continue;
                }
                if let Some(state) = self.sr_virtual_object_state(new_pc) {
                    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SCALAR_DEOPT").is_some()
                    {
                        eprintln!(
                            "[DBG_SCALAR_DEOPT] x64 emit VirtualObject local={i} new_pc={new_pc} \
                             class_id={} fields={} at bci={bci}",
                            state.class_id, state.num_fields
                        );
                    }
                    sr_emitted.insert(new_pc);
                    locals.push(FrameValue::VirtualObject(state));
                    continue;
                }
                // Metadata missing → fall through to the safe re-run encoding
                // (the gate excludes such methods, but never emit a bogus ref).
                locals.push(FrameValue::Unsupported);
                continue;
            }
            let is_oop = i < 64 && (oop_mask & (1u64 << i)) != 0;
            let reg = self.reg_for_local(i);
            let xmm = self.xmm_for_local(i);
            let off = self.local_offset(i);
            let fv = if is_oop {
                // The precise oop mask is the authority for ref-typed slots. A
                // SPILLED ref → `StackSlotRef`; a REGISTER-resident ref →
                // `RegisterRef(r)` (deopt-osr P2 trap-2). Both resolve to the raw
                // heap pointer captured in-stub at the guard (the GPR is spilled
                // into `SavedRegisters.gpr`), and the resume builds a GC-tracked
                // `Value::Object` — NOT the truncating `Register(r)`/`Int`, which
                // would also drop the oop from the GC root scan (a moving-GC UAF).
                if let Some(r) = reg {
                    crate::deopt::FrameValue::RegisterRef(r)
                } else {
                    frame_value_for_slot(reg, xmm, off, true)
                }
            } else if let Some(&kind) = self.local_kinds.get(i) {
                // Non-oop slot, width-typed from the classifier (deopt-osr P2).
                // A slot the whole-method classifier had to call `Ambiguous`
                // gets one more chance from the per-bci reaching-kind dataflow:
                // legal slot reuse across two disjoint live ranges is ambiguous
                // for the METHOD and usually not here. `kind_at` only ever
                // answers a concrete NON-ref kind, so the oop mask (which ran
                // above) keeps sole authority over ref-typed slots.
                let kind = if matches!(kind, LocalKind::Ambiguous) {
                    self.local_kinds_refined.kind_at(bci, i).unwrap_or(kind)
                } else {
                    kind
                };
                typed_local_frame_value(reg, xmm, off, kind)
            } else {
                // No kind table (gate off / unmapped) — Phase-A int/provenance.
                frame_value_for_slot(reg, xmm, off, false)
            };
            locals.push(fv);
        }

        // `CRATONVM_DBG_EXCFRAME=1` also dumps the WHOLE published locals vector,
        // not just the slots the liveness mask dropped. A slot that survives
        // liveness can still be published with the wrong *type source* — a
        // ref-typed local that the oop mask does not claim comes out as
        // `Register`/`StackSlot`, which the resume sink turns into a
        // `Value::Int`, and reading it back with `aload` then behaves as null.
        // That failure is invisible in the DROP lines alone, so print the
        // provenance the snapshot actually chose for every slot.
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_EXCFRAME").is_some() {
            eprintln!(
                "[excframe] FRAME method={} bci={bci} reason={reason:?} oop_reached={oop_reached} \
                 oop_mask={oop_mask:#x} locals={:?}",
                self.method_key, locals,
            );
        }

        // Operand stack (empty at a BCE loop header; non-empty at OSR-exit loop
        // boundaries): map each live entry's StackSlot location to a FrameValue.
        //
        // FU2 — the abstract operand stack has NO per-entry width source (unlike
        // locals, which `local_kinds` types). A non-oop `Frame`/GPR stack slot
        // could be a cat-1 `int` OR a cat-2 `long` / spilled FP, and the snapshot
        // can't tell. `uses_long_float_double` is the sound method-level gate: when
        // the method touches no `long`/`float`/`double`, every non-oop stack slot
        // is provably a cat-1 `int`/`ref` and keeps its precise encoding; otherwise
        // such slots are recorded `Unsupported` (re-run) rather than risk a
        // truncated `long` / mistyped FP on resume. (An XMM-resident stack slot is
        // FP but float-vs-double is unknown here, so it is always `Unsupported`.)
        // Pure-int/ref methods (the BCE pilot) are unaffected.
        let wide_fp = self.uses_long_float_double;
        let n = self.stack.len().min(self.stack_oop_marks.len());
        // deopt-osr indy-arg-types fix: at an invokedynamic trap bci, the top
        // `indy_arg_types.len()` stack entries are this call's own arguments,
        // whose types are known PRECISELY from its descriptor — unlike the
        // rest of the operand stack, which has no per-entry width source and
        // falls back to the coarse `wide_fp` gate below. `indy_arg_base` is
        // the first (deepest) global stack index these tags cover; `None`
        // outside an indy trap bci (the ordinary case), so behavior there is
        // unchanged. See `indy_stack_arg_types`'s doc comment.
        //
        // Generalized 2026-08-03 from invokedynamic to EVERY invoke: the same
        // "top `tags.len()` entries are this call's arguments" fact holds at a
        // `ReceiverTypeChanged` guard, which snapshots before the arg pops. See
        // `invoke_stack_arg_types`.
        let indy_arg_types = self
            .indy_stack_arg_types
            .get(&bci)
            .or_else(|| self.invoke_stack_arg_types.get(&bci))
            // Alignment cross-check. The tags are positional, so a vector that
            // does not line up with the abstract stack would type the WRONG
            // entries — a truncated long or a mistyped FP on resume, which is
            // precisely the silent corruption the coarse `Unsupported` fallback
            // exists to avoid. The oop marks are an INDEPENDENT per-entry
            // opinion the emitter maintains for the GC, so make the two agree
            // or use neither: every tagged entry must be a ref exactly when its
            // mark says ref. Any disagreement discards the whole vector for
            // this bci and leaves the pre-existing behaviour in place.
            //
            // This catches a stale/misaligned vector for any call whose
            // signature mixes references and primitives, and any instance call
            // (its receiver is a ref sitting immediately below the tags).
            .filter(|tags| {
                let base = n.saturating_sub(tags.len());
                tags.len() <= n
                    && tags.iter().enumerate().all(|(k, &tag)| {
                        let is_ref_tag = tag == b'L';
                        self.stack_oop_marks
                            .get(base + k)
                            .is_none_or(|&m| m == is_ref_tag)
                    })
            });
        let indy_arg_base = indy_arg_types.map(|tags| n.saturating_sub(tags.len()));
        // The operand stack's width source (`x64::stack_kinds`), admitted only
        // when it agrees with the emitter about this bci's live stack on two
        // independent counts:
        //
        //   * DEPTH — the analysis derives it from the JVMS stack effects, the
        //     emitter from running its own opcode handlers. A modelling error
        //     that shifts the stack changes the depth, and a positional tag
        //     vector applied at the wrong offset is exactly the truncated-long
        //     corruption the coarse fallback exists to prevent.
        //   * REF-NESS — every entry the analysis calls a reference must be one
        //     the emitter's oop mark also calls a reference, and vice versa.
        //     The marks are maintained for the GC, so this is a second opinion
        //     with a different provenance, and it catches an off-by-one that
        //     happens to preserve depth.
        //
        // Either disagreement discards the whole vector for this bci and leaves
        // the pre-existing encoding in place.
        let raw_kinds = self.stack_kinds.get(bci);
        let stack_kinds = raw_kinds
            .filter(|kinds| kinds.len() == n)
            .filter(|kinds| {
                kinds.iter().enumerate().all(|(i, k)| {
                    let analysis_says_ref = *k == super::stack_kinds::StackKind::Ref;
                    let unknown = *k == super::stack_kinds::StackKind::Unknown;
                    unknown || self.stack_oop_marks[i] == analysis_says_ref
                })
            });
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_STACK_KINDS").is_some() {
            // Which of the three outcomes happened is the whole diagnosis when
            // a snapshot stays `Unsupported`: no answer at this bci (the
            // analysis poisoned upstream), an answer the emitter's depth or oop
            // marks contradict (a modelling bug), or an accepted answer that is
            // simply `Unknown` at the blocking index.
            eprintln!(
                "[stack-kinds] {} bci={bci} emitter_depth={n} analysis={:?} accepted={}",
                self.method_key,
                raw_kinds,
                stack_kinds.is_some(),
            );
        }
        let mut stack = Vec::with_capacity(n);
        for i in 0..n {
            let is_oop = self.stack_oop_marks[i];
            let indy_tag = indy_arg_types.zip(indy_arg_base).and_then(|(tags, base)| {
                if i >= base {
                    tags.get(i - base).copied()
                } else {
                    None
                }
            });
            stack.push(match &self.stack[i] {
                StackSlot::Frame(off) => {
                    if is_oop {
                        FrameValue::StackSlotRef(-*off)
                    } else if indy_tag == Some(b'J') {
                        FrameValue::StackSlotLong(-*off)
                    } else if indy_tag == Some(b'D') {
                        FrameValue::StackSlotDouble(-*off)
                    } else if indy_tag == Some(b'F') {
                        FrameValue::StackSlotFloat(-*off)
                    } else if indy_tag == Some(b'I') || !wide_fp {
                        FrameValue::StackSlot(-*off)
                    } else {
                        // Previously always `Unsupported`. The typed stack is
                        // asked here and only here, so every encoding the old
                        // code produced it still produces.
                        match stack_kinds.and_then(|kinds| kinds.get(i)) {
                            Some(super::stack_kinds::StackKind::Long) => {
                                FrameValue::StackSlotLong(-*off)
                            }
                            Some(super::stack_kinds::StackKind::Double) => {
                                FrameValue::StackSlotDouble(-*off)
                            }
                            Some(super::stack_kinds::StackKind::Float) => {
                                FrameValue::StackSlotFloat(-*off)
                            }
                            Some(super::stack_kinds::StackKind::Int) => {
                                FrameValue::StackSlot(-*off)
                            }
                            // `Ref` cannot reach here: `is_oop` handled it
                            // above, and the ref-agreement filter guarantees
                            // the two never disagree.
                            _ => FrameValue::Unsupported,
                        }
                    }
                }
                // A register-resident operand: a ref → `RegisterRef` (GC-tracked
                // Object on resume); a non-oop slot is a cat-1 `Register` (Int)
                // only when the method has no wide/FP value that could occupy it
                // (or an indy-arg tag proves it's actually int/long).
                //
                // An FP indy arg in a GPR is a contradiction (the FP tier keeps
                // `float`/`double` in XMM or a spill slot), so it stays
                // `Unsupported` — exactly what `typed_local_frame_value` does
                // for a `LocalKind::Float`/`Double` that claims a GPR home.
                StackSlot::CalleeSaved(r) | StackSlot::Scratch(r) => {
                    if is_oop {
                        FrameValue::RegisterRef(*r)
                    } else if indy_tag == Some(b'J') {
                        FrameValue::RegisterLong(*r)
                    } else if indy_tag == Some(b'I') || !wide_fp {
                        FrameValue::Register(*r)
                    } else {
                        // Same upgrade as the frame-slot arm above. A GPR-homed
                        // `Float`/`Double` stays `Unsupported`: the FP tier
                        // keeps those in an XMM or a spill slot, so a wide-FP
                        // kind claiming a GPR home is a contradiction, not a
                        // value to encode — the same judgement
                        // `typed_local_frame_value` makes for locals.
                        match stack_kinds.and_then(|kinds| kinds.get(i)) {
                            Some(super::stack_kinds::StackKind::Long) => {
                                FrameValue::RegisterLong(*r)
                            }
                            Some(super::stack_kinds::StackKind::Int) => FrameValue::Register(*r),
                            _ => FrameValue::Unsupported,
                        }
                    }
                }
                // An XMM-resident operand is FP, but float-vs-double is not
                // recoverable from the abstract stack alone — EXCEPT at an
                // invokedynamic trap bci, where the call site's own descriptor
                // types each of its arguments exactly.
                StackSlot::Xmm(n) => match indy_tag {
                    Some(b'D') => FrameValue::XmmDouble(*n),
                    Some(b'F') => FrameValue::XmmFloat(*n),
                    _ => FrameValue::Unsupported,
                },
            });
        }

        // Phase C: scalar monitors held at this bci → `MonitorInfo` referencing
        // the re-materialized object by `VirtualObjectRef(new_pc)` (the same id the
        // locals' `VirtualObject` defines), so the resume re-acquires the elided
        // lock `lock_depth` times. Empty unless a `synchronized(scalarObj)` block is
        // open here.
        let monitors: Vec<crate::deopt::MonitorInfo> = self
            .sr_monitor_at
            .get(&bci)
            .map(|held| {
                held.iter()
                    .map(|&(new_pc, depth)| crate::deopt::MonitorInfo {
                        object: FrameValue::VirtualObjectRef(new_pc),
                        lock_depth: depth,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let point = DeoptimizationPoint {
            native_offset,
            // Interpreter-bci space; see "THE COORDINATE CHANGE" above.
            bci: resume_bci,
            reason,
            action: DeoptAction::Reinterpret,
            // Behaviour-preserving: `for_reason` is exactly the per-`DeoptReason`
            // prose convention this site already relied on, now written down in
            // one place instead of being inferred by each resume sink.
            semantics: crate::deopt::ResumeSemantics::for_reason(reason),
            speculation_id: 0,
            frame_state: FrameState {
                // Deopt-frame identity (jit-invokedynamic-groovy-regression root
                // cause): bake this method's `"<class>.<method>:<descriptor>"`
                // key into every snapshot so the VM-side resume sinks can verify
                // a stashed `ReconstructedFrame` actually belongs to the method
                // they are about to resume. Without it, a trap in a NESTED
                // compiled callee propagated the `i64::MIN` sentinel up through
                // its compiled callers' epilogue bails, and the OUTERMOST
                // interpreter sink consumed the (identity-less) inner frame as
                // if it were the outer method's — materializing the outer
                // method's frame with the inner method's locals/stack/bci, i.e.
                // resuming arbitrary bytecode with a foreign frame. Empty only
                // for legacy/test wrappers that pass no key (the consumers
                // treat an empty key as "never matches" → safe re-run).
                method_key: self.method_key.clone(),
                // Interpreter-bci space; see "THE COORDINATE CHANGE" above.
                bci: resume_bci,
                locals,
                stack,
                monitors,
                caller: None,
            },
        };
        // Record a stable boxed copy (the frame-deopt stub bakes it as arg0) and
        // the by-value point (find_deopt_point / iteration). The Box payload does
        // not move when `deopt_boxes` reallocs or when it is moved into
        // `CompiledMethod::_deopt_point_boxes` at finalize (and is leaked on
        // Drop), so a baked imm64 of this pointer outlives the emitted code.
        // Capture the heap payload's address with `addr_of!` BEFORE moving the
        // Box into the Vec — pushing the Box (a pointer) does not relocate its
        // payload, so this is the same address `&**deopt_boxes.last()` would
        // yield, without a `.unwrap()` (keeps this hot codegen path panic-free).
        let boxed = Box::new(point.clone());
        let box_ptr: *const crate::deopt::DeoptimizationPoint = std::ptr::addr_of!(*boxed);
        self.deopt_boxes.push(boxed);
        self.deopt_points.push(point);
        // The emitter pc this point was recorded at, kept in step with
        // `deopt_points` so the coordinate change above can be re-derived and
        // checked at finalize rather than trusted.
        self.deopt_point_pcs.push(bci);
        box_ptr
    }

    /// Republish this caller's frame after a raw JIT-to-JIT CALL.
    ///
    /// A compiled callee records its own RBP in the precise-root mirror in its
    /// prologue.  On return there is no Rust boundary to restore the caller's
    /// mirror, leaving a later GC to interpret the caller with the already
    /// returned callee's frame pointer.  That stale frame was the source of
    /// the IVFKnn native-control-transfer corruption.  Preserve the Java
    /// result in RAX while calling the same frame-record hook used by the
    /// prologue.  The extra eight bytes maintain ABI alignment after PUSH; on
    /// Windows `stack_arg_block_size(0)` additionally reserves shadow space.
    /// After an INLINE call to a cached compiled callee: if it returned the
    /// `i64::MIN` deopt/exception sentinel, hand it to
    /// `jit_service_callee_deopt` so the callee's stashed frame is resumed
    /// here, at the call site that actually made the call.
    ///
    /// Emitted only on the direct-entry arms. Without it the sentinel reaches
    /// the caller's own epilogue as if the CALLER had deopted, and the callee's
    /// reconstructed frame is left for an unrelated sink to mis-attribute —
    /// see the helper's doc comment for the H2 `MVMap`/`DataType.read` case
    /// this was found on.
    ///
    /// Cost on the hit path is a `MOV imm64` + `CMP` + a not-taken `JNE`; the
    /// call is on the sentinel branch only. Emits nothing when the runtime
    /// offers no helper (unit-test compiles), which restores the previous
    /// behaviour exactly.
    /// CRATONVM_DBG_DEOPT — name a direct JIT-to-JIT call site that gets NO
    /// callee-deopt service check.
    ///
    /// Such a site is where an orphaned deopt frame is born: the callee traps,
    /// stashes a frame keyed to ITSELF, and returns the `i64::MIN` sentinel;
    /// with no check here the sentinel reaches this method's shared
    /// exception-check stub, which reloads the sentinel and returns — so the
    /// stash travels up to a consumer that cannot attribute it. Printing the
    /// site, and WHICH of the two preconditions was missing, is what turns "an
    /// orphan appeared" into a named call site.
    pub(super) fn dbg_unserviced_direct_call(
        &self,
        kind: &str,
        pc: usize,
        has_info: bool,
        has_args_base: bool,
    ) {
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_none() {
            return;
        }
        eprintln!(
            "[cratonvm-deopt] direct {kind} call at {}#{pc} has NO callee-deopt service \
             (info={has_info} args_base={has_args_base})",
            self.method_key,
        );
    }

    pub(super) fn emit_inline_callee_deopt_check(
        &mut self,
        info: *const crate::JitInvokeInfo,
        n: usize,
        args_base_offset: i32,
    ) {
        let helper = self.helpers.service_callee_deopt;
        if helper == 0 {
            return;
        }
        // Bisect lever `CRATONVM_JIT_SP_IC_DEOPT_CHECK`: `0` drops the check
        // everywhere, `void` drops it only where the callee's descriptor
        // returns VOID.
        //
        // The `void` case is the interesting one. This compares the raw return
        // REGISTER against `i64::MIN`, and a void callee leaves in RAX whatever
        // its last helper call returned — there is no return value to compare.
        // A false positive is not a wasted helper call: the servicing helper
        // DRAINS the thread's whole pending-signal record. `.done` still runs
        // `emit_post_invoke_exception_check`, so a genuinely-throwing void
        // callee is still caught by the caller's own drain with this off.
        match crate::sp_ic_deopt_check_mode() {
            crate::SpIcDeoptCheck::Off => return,
            crate::SpIcDeoptCheck::SkipVoid => {
                // SAFETY: `info` is a live `JitInvokeInfo` for the duration of
                // this compilation — the same contract every other read of it
                // on this path relies on.
                if !info.is_null() && unsafe { (*info).return_type } == b'V' {
                    return;
                }
            }
            crate::SpIcDeoptCheck::On => {}
        }
        // MOV R11, imm64(i64::MIN)  — 49 BB + imm64.
        self.buf.emit(&[0x49, 0xBB]);
        self.buf.emit(&i64::MIN.to_le_bytes());
        // CMP RAX, R11  — REX.WR + 39 /r + ModRM(11, R11, RAX).
        self.buf.emit(&[0x4C, 0x39, 0xD8]);
        // JNE rel8 → skip the servicing call. Patched once the body length is
        // known; the body is a fixed short sequence well inside rel8 range.
        self.buf.emit(&[0x75, 0x00]);
        let jne_patch = self.buf.pos() - 1;
        let body_start = self.buf.pos();

        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        // Cast: function pointer for JIT call target
        self.emit_mov_imm64(ARG_REGS[1], info as i64);
        if n > 0 {
            // Cast: x86-64 immediate encoding
            let buf_start = args_base_offset + ((n as i32) - 1) * 8;
            self.emit_lea_frame_slot(ARG_REGS[2], buf_start);
        } else {
            self.emit_xor_reg_self(ARG_REGS[2]);
        }
        // Cast: x86-64 immediate encoding
        self.emit_mov_imm32_sx(ARG_REGS[3], n as i32);
        self.emit_call_absolute(helper);

        // Widening: usize offset -> i64 (no truncation; for rel/displacement math)
        let rel = (self.buf.pos() as i64) - (body_start as i64);
        debug_assert!(
            (0..=i64::from(i8::MAX)).contains(&rel),
            "inline callee-deopt check body overflowed rel8 ({rel} bytes)"
        );
        // `u8::try_from` was the wrong range: it accepts 128..=255, which the
        // CPU reads as a NEGATIVE rel8 — a backward branch into the body this
        // jump exists to skip, i.e. the same shape as the PIC cascade's
        // `JNE -128`. `patch_rel8_or_bail` range-checks against `i8` and marks
        // the buffer (compile discarded) when it does not fit.
        Self::patch_rel8_or_bail(&mut self.buf, jne_patch, rel);
    }

    /// Emit out-of-line bounds check failure stubs at the end of the method.
    ///
    /// Each bytecode array-access site gets its own cold landing pad. Besides
    /// the index, length, and array pointer already live at the failing check,
    /// the pad passes the originating bytecode PC to `jit_throw_aioobe`.
    ///
    /// Do not coalesce these pads. A shared landing pad makes a live AIOOBE
    /// impossible to attribute to one of the method's array accesses: the
    /// helper return PC identifies only the common pad, while the machine
    /// instructions preceding the pad are merely the last-emitted main-code
    /// block and need not be the branch that jumped there.
    pub(super) fn emit_bounds_check_stubs(&mut self) {
        if self.bounds_check_stubs.is_empty() {
            return;
        }

        // Clone the small metadata vector so emitting pads can mutably borrow
        // `self`. Unrolled copies keep the same bci but have distinct branch
        // offsets; a separate pad for each remains unambiguous.
        let sites = self.bounds_check_stubs.clone();
        for (patch_off, bc_pc) in sites {
            // `bc_pc` is an EMITTER pc. `jit_throw_aioobe`'s 4th argument is
            // reported as this method's bytecode index, so it must be an
            // interpreter bci — the identity unless this compile is emitting
            // rewritten bytecode. See `Compiler::bci_provenance`.
            let bc_pc = self.orig_bci(bc_pc);
            let stub_offset = self.buf.pos();

            // At this point RAX=array pointer, RCX=index, R10D=array length.
            // Set up jit_throw_aioobe(index, length, array_ptr, bytecode_pc).
            #[cfg(target_os = "windows")]
            {
                // Windows: arg1=RCX, arg2=RDX, arg3=R8, arg4=R9.
                self.buf.emit(&[0x49, 0x89, 0xC0]); // MOV R8, RAX
                self.buf.emit(&[0x4C, 0x89, 0xD2]); // MOV RDX, R10
                self.emit_mov_imm32_sx(R9, bc_pc as i32);
            }
            #[cfg(not(target_os = "windows"))]
            {
                // SysV: arg1=RDI, arg2=RSI, arg3=RDX, arg4=RCX.
                self.rex_w();
                self.buf.emit_byte(0x8B);
                self.modrm_reg(RDX, RAX); // MOV RDX, RAX
                self.rex_w();
                self.buf.emit_byte(0x8B);
                self.modrm_reg(RDI, RCX); // MOV RDI, RCX
                self.buf.emit(&[0x4C, 0x89, 0xD6]); // MOV RSI, R10
                self.emit_mov_imm32_sx(RCX, bc_pc as i32);
            }

            // Return the sentinel through this method's epilogue; the
            // interpreter materializes and routes the Java exception.
            self.emit_call_absolute(self.helpers.throw_aioobe);
            self.emit_epilogue();

            let rel32 = (stub_offset as i32) - (patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
            self.buf.try_patch_i32(patch_off, rel32).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
        }
    }

    /// Round-8 CRIT fix (audit `round8-jit.md`): emit the inline null-check
    /// failure stubs for inline array loads / stores / `arraylength`. All `JZ`
    /// branches recorded by [`emit_null_check_array_store`] /
    /// [`emit_null_check_array_load`] are patched to point at a stub.
    ///
    /// JEP 358 (helpful NPE) inline-codegen path: each recorded entry carries an
    /// [`npe_action`] code for its trapping opcode. We emit ONE stub per
    /// *distinct* code present in the method, so a null `iaload` and a null
    /// `castore` deopt through separate stubs that report "Cannot load from int
    /// array" vs "Cannot store to char array" respectively. Each stub loads its
    /// action code into the first argument register and calls
    /// `helpers.jit_npe_with_action(code)` (which sets `JIT_PENDING_NPE` *with*
    /// the action and the deopt flag), then loads `i64::MIN` into RAX and runs
    /// the method epilogue. The interpreter's post-JIT path drains the NPE flag
    /// on every JIT return and surfaces the (gated) action-only message.
    ///
    /// This replaced the prior single shared stub that called
    /// `helpers.bastore(0)` — which fabricated `ASTORE_BYTE` ("Cannot store to
    /// byte array") for *every* inline array opcode regardless of element type
    /// or load/store direction. The fast path (`TEST RAX,RAX; JZ`) is unchanged;
    /// only these cold out-of-line stubs differ. The number of stubs is bounded
    /// by the distinct element kinds the method touches (≤ ~18), all cold.
    ///
    /// `null_check_store_stubs` is left intact (not drained): a later
    /// `has_dispatch` computation reads `!is_empty()` to force the
    /// dispatch-aware entry path that drains the NPE flag.
    pub(super) fn emit_null_check_store_stubs(&mut self) {
        if self.null_check_store_stubs.is_empty() {
            return;
        }

        // Clone the (action, patch_offset) entries so we can borrow `self`
        // mutably (emit_call_absolute / emit_epilogue) while iterating. The
        // field itself stays populated for the `has_dispatch` check downstream.
        let entries = self.null_check_store_stubs.clone();

        // Distinct action codes in first-seen order (small set, ≤ ~18). Emit one
        // stub per code; all JZ sites with that code patch to it.
        let mut emitted: Vec<u8> = Vec::new();
        for &(action, _) in &entries {
            if emitted.contains(&action) {
                continue;
            }
            emitted.push(action);

            let stub_offset = self.buf.pos();

            // MOV <arg1>, action  — `MOV r32, imm32` zero-extends to the full
            // 64-bit argument register, so the `i64` code arg is exactly the
            // (always-non-negative) action code.
            #[cfg(target_os = "windows")]
            {
                // Windows arg1 = RCX → MOV ECX, imm32 (B9 id).
                self.buf.emit_byte(0xB9);
            }
            #[cfg(not(target_os = "windows"))]
            {
                // SysV arg1 = RDI → MOV EDI, imm32 (BF id).
                self.buf.emit_byte(0xBF);
            }
            self.buf.emit(&(action as u32).to_le_bytes()); // Cast: x86-64 imm32

            // CALL jit_npe_with_action (absolute). Sets JIT_PENDING_NPE + the
            // action code + the deopt flag. RAX is clobbered by the call.
            self.emit_call_absolute(self.helpers.jit_npe_with_action);

            // MOV RAX, i64::MIN  — deopt sentinel so the interpreter's post-JIT
            // path treats this as a deopt return and runs the NPE drain.
            // 48 B8 <imm64>
            self.buf.emit(&[0x48, 0xB8]);
            self.buf.emit(&(i64::MIN as u64).to_le_bytes()); // Cast: x86-64 immediate encoding

            // Standard method epilogue: restore callee-saved regs and return.
            self.emit_epilogue();

            // Patch every recorded JZ branch with THIS action to this stub.
            for &(a, patch_off) in &entries {
                if a != action {
                    continue;
                }
                let rel32 = (stub_offset as i32) - (patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
                self.buf.try_patch_i32(patch_off, rel32).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
            }
        }
    }

    /// Emit the post-invoke pending-exception guard.
    ///
    /// Called immediately after a JIT-dispatched invoke helper
    /// (`invoke_dispatch` / `invoke_virtual_mic`) returns, with the
    /// helper's return value still in RAX. The dispatch helpers return
    /// `i64::MIN` when the callee threw a Java exception (the exception
    /// object is stashed in the thread-local `JIT_PENDING_EXCEPTION`).
    ///
    /// Without this guard the JIT pushed the bogus `0` return value and
    /// kept executing — running arbitrary follow-on bytecode against a
    /// null/garbage value and masking the real exception with a downstream
    /// secondary failure (the Jetty `Main.main` "getClasspath on null"
    /// miscompile: `processCommandLine` threw a `SecurityException`, the
    /// dispatch helper returned 0, `astore_3` stored null, and the later
    /// `start(null)` call NPE'd — hiding the genuine exception).
    ///
    /// The guard `CMP RAX, i64::MIN; JE rel32` branches to a single
    /// shared out-of-line stub (`emit_exception_check_stub`) that loads
    /// the `i64::MIN` deopt sentinel and runs the method epilogue. The
    /// interpreter's post-JIT path (`vm/src/runtime/interpreter.rs`,
    /// `take_jit_pending_exception`) then routes the stashed exception
    /// through this method's exception table.
    ///
    /// RAX is caller-saved and already clobbered by the dispatch call, so
    /// using R10 as the `i64::MIN` scratch is safe here.
    ///
    /// `ret_type` is the callee's JVM return-descriptor byte. For an int/ref/
    /// void return (`i64::MIN` is never a legitimate result) the check is the
    /// plain `CMP RAX, i64::MIN; JE bail` and stays byte-identical to before.
    /// For a `J`/`D` (long/double) return a legitimate `Long.MIN_VALUE` result
    /// is bit-identical to the sentinel, so on the (rare) `RAX == i64::MIN`
    /// branch we peek the out-of-band signal via `jit_dispatch_threw`
    /// (`self.helpers.dispatch_threw`): bail only when a genuine exception/deopt
    /// is pending, else restore the real value and continue. This is what makes
    /// dispatching a `J`/`D`-returning callee sound (a `Pack.bigEndianToLong`
    /// SHA-512 word == `0x8000_0000_0000_0000` would otherwise be misread as a
    /// deopt and the caller would silently bail mid-method).
    /// Is `pc` inside any of this method's exception-table protected ranges?
    ///
    /// Only the sibling tail-call needs this. Every other lowering keeps this
    /// frame alive across the call and routes a pending exception through the
    /// shared stub, which re-enters the interpreter at the throwing bci where
    /// the method's own handler table applies. A tail-call has already run
    /// `emit_epilogue_without_ret` by the time the callee executes, so there is
    /// no frame left to catch into and the exception escapes to this method's
    /// caller instead — the wrong handler, silently.
    pub(super) fn pc_is_protected(&self, pc: usize) -> bool {
        // `pc` is an EMITTER pc; `protected_ranges` was published by the
        // bytecode front-end from the method's INTERPRETER exception table and
        // is deliberately not rewritten. Ask the question about the original
        // instruction: under a loop rewrite every copy of a bytecode inside a
        // `try` maps back to the one bci the range covers, which is the answer
        // each copy needs. Identity — and byte-identical — on an ordinary
        // compile, where `orig_bci` is `pc`.
        // Cast: bci fits u32
        let pc = self.orig_bci(pc) as u32;
        self.protected_ranges
            .iter()
            .any(|(start, end)| pc >= *start && pc < *end)
    }

    /// The interpreter bci an emitter PC belongs to.
    ///
    /// The identity on every ordinary compile (`bci_provenance == None`), so
    /// the four call sites are byte-identical there. When this compile is
    /// emitting rewritten bytecode it is `LoopXform::bci_at`, which is total
    /// over the rewritten method — `plan_bytecode_loop_xform` refuses any
    /// transform whose `provenance_is_total()` does not hold, so the `None`
    /// arm of the lookup is unreachable for an in-range PC. It is still
    /// written to fall back to `pc` rather than panic, because these callers
    /// run inside the no-panic emit path.
    pub(super) fn orig_bci(&self, pc: usize) -> usize {
        match &self.bci_provenance {
            // Widening: u32 -> usize
            Some(map) => map.get(pc).map(|&b| b as usize).unwrap_or(pc),
            None => pc,
        }
    }

    pub(super) fn emit_post_invoke_exception_check(&mut self, ret_type: u8) {
        // The simulated operand stack at this point is the state *after* the
        // instruction which made the fallible call: every invoke lowering has
        // already removed its receiver and arguments before emitting the ABI
        // call above.  A frame-deopt snapshot must therefore resume at that
        // instruction's successor, not at `dbg_last_pc` itself.  Resuming at
        // the invoke would make the interpreter try to consume the already
        // removed operands (TransactionUtil.wrapInTransaction's
        // `Consumer.accept` was the concrete failure: the reconstructed frame
        // resumed pc=25 with an empty stack, and the virtual dispatch cache
        // underflowed after advancing to pc=30).
        //
        // Every caller invokes this helper after lowering a bytecode operation.
        // `dbg_last_op` provides the exact encoded width for the only variable
        // length invoke forms; all other supported fallible helpers here use
        // the ordinary three-byte CP form or a one-byte operation.  Keeping the
        // original PC for non-invoke operations avoids changing their exception
        // routing semantics.
        // ...which is why an ordinary RESUME snapshot keys on the successor.
        // A reason-9 frame is not a resume point: it is consumed by
        // `route_jit_signal_exception`, which uses the frame's bci as the THROW
        // pc for the handler's `[start_pc, end_pc)` range test. javac routinely
        // ends a protected range exactly at the successor of its last invoke
        // (`JSONValue.toJSONString`: range [8,14), invoke at pc 11), so keying
        // this snapshot on the successor put the throw OUTSIDE the very handler
        // that had to run and the exception escaped its own catch block. Key it
        // on the throwing instruction itself.
        //
        // The frame is also only useful where this method's exception table can
        // catch at all: outside every protected range the throw propagates to
        // the caller, so the shared sentinel-only exit is both correct and
        // cheaper, and the stash stays quiet on straight-line invokes.
        let throw_bci = self.dbg_last_pc;
        let precise_exc_stub = self.precise_exception_frames && self.pc_is_protected(throw_bci);
        if precise_exc_stub && !self.exc_frame_box_ptr_by_bci.contains_key(&throw_bci) {
            let box_ptr = self.build_and_record_deopt_point(
                throw_bci,
                crate::deopt::DeoptReason::PendingException,
            );
            self.exc_frame_box_ptr_by_bci.insert(throw_bci, box_ptr);
        }
        // MOV R10, i64::MIN  (49 BA <imm64>)
        self.buf.emit(&[0x49, 0xBA]);
        self.buf.emit(&(i64::MIN as u64).to_le_bytes()); // Cast: x86-64 immediate encoding
                                                         // CMP RAX, R10  (4C 39 D0)
        self.buf.emit(&[0x4C, 0x39, 0xD0]);
        if matches!(ret_type, b'J' | b'D' | b'F') {
            // JNE .keep (0F 85 rel32) — common path: not the sentinel, keep RAX.
            self.buf.emit(&[0x0F, 0x85]);
            let keep_patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            // Cold: RAX == i64::MIN. Peek whether a genuine exception/deopt is
            // pending — MOV RAX, dispatch_threw ; CALL RAX (RAX = 0/1). The
            // imm64-baked address is loop-unroll copy-safe (no rel32 to track).
            self.emit_mov_imm64(RAX, self.helpers.dispatch_threw as i64); // Cast: helper address
            self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                                          // TEST RAX, RAX (48 85 C0) — ZF=1 iff no signal (legit value).
            self.buf.emit(&[0x48, 0x85, 0xC0]);
            // Restore the legit `Long.MIN_VALUE` into RAX for the keep path; the
            // shared bail stub reloads `i64::MIN` itself, so this is harmless on
            // the bail path. MOV does not disturb ZF.
            self.emit_mov_imm64(RAX, i64::MIN);
            // JNE bail_stub (0F 85 rel32) — pending ⇒ propagate the sentinel.
            self.buf.emit(&[0x0F, 0x85]);
            let patch_offset = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
            if precise_exc_stub {
                self.deopt_stubs.push((patch_offset, throw_bci, 9));
            } else {
                self.exception_check_stubs
                    .push((patch_offset, self.dbg_last_pc));
            }
            // .keep: patch the JNE above to land here (self-relative ⇒ copy-safe).
            let keep_off = self.buf.pos();
            let rel = (keep_off as i32) - (keep_patch as i32 + 4); // Cast: rel32 displacement
            self.buf.try_patch_i32(keep_patch, rel).ok(); // on Err sets buf.overflowed; compile bails
        } else {
            // JE rel32 → shared exception-check stub (patched later)
            self.buf.emit(&[0x0F, 0x84]);
            let patch_offset = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
            if precise_exc_stub {
                self.deopt_stubs.push((patch_offset, throw_bci, 9));
            } else {
                self.exception_check_stubs
                    .push((patch_offset, self.dbg_last_pc));
            }
        }
    }

    /// Emit the post-allocation OOM guard, immediately after an allocation
    /// helper (`newarray` / `new_object` / `anewarray_object`) returns with its
    /// result still in RAX. Those helpers return the `0`/null sentinel on heap
    /// exhaustion (after stashing a `java/lang/OutOfMemoryError` in
    /// `JIT_PENDING_EXCEPTION` — see `jit_alloc_oom`). A successful allocation is
    /// never null, so `TEST RAX,RAX; JZ` distinguishes the OOM case.
    ///
    /// Without this guard the JIT pushed the null result onto the operand stack
    /// and kept executing — the very next `arraylength` / `getfield` / array
    /// store dereferenced it and SIGSEGV'd (read at `[null + offset]`) before
    /// the method could return and drain the pending OOME. Branching to the same
    /// shared stub the invoke guard uses (`emit_exception_check_stub`) loads the
    /// `i64::MIN` deopt sentinel and runs the epilogue; the interpreter's
    /// post-JIT drain then throws the stashed OOME through the method's
    /// exception table (catchable, matching the interpreter's allocation paths).
    ///
    /// **Inside a protected range this guard publishes a precise exceptional
    /// frame**, exactly as `emit_post_invoke_exception_check` does, instead of
    /// branching to the shared sentinel-only stub. That is what makes `new`
    /// (0xbb) admissible to RBC.6 - see `precise_alloc_ops_enabled` in
    /// `jit/src/lib.rs` for the argument that this is the whole obligation, and
    /// the netty adaptive-allocator throughput page for the method it was
    /// refusing (`AdaptivePoolingAllocator$Magazine.allocate`,
    /// `reason=rbc6-handler-reads-unsafe-local(pc=338,op=0xbb)`).
    ///
    /// The bci keyed here is the ALLOCATING instruction's own, not its
    /// successor: a reason-9 frame is consumed by `route_jit_signal_exception`,
    /// which range-tests the bci as the THROW pc against `[start_pc, end_pc)`.
    /// `emit_post_invoke_exception_check` carries the full argument for that
    /// choice, and javac ends a protected range at the successor of its last
    /// instruction often enough that keying on the successor puts the throw
    /// outside its own handler.
    pub(super) fn emit_post_alloc_oom_check(&mut self) {
        // Same shape as `emit_post_invoke_exception_check`: a frame is only
        // useful where this method's own exception table can catch, so outside
        // every protected range the cheaper shared sentinel exit stays.
        let throw_bci = self.dbg_last_pc;
        let precise_exc_stub = self.precise_exception_frames && self.pc_is_protected(throw_bci);
        if precise_exc_stub && !self.exc_frame_box_ptr_by_bci.contains_key(&throw_bci) {
            let box_ptr = self.build_and_record_deopt_point(
                throw_bci,
                crate::deopt::DeoptReason::PendingException,
            );
            self.exc_frame_box_ptr_by_bci.insert(throw_bci, box_ptr);
        }
        // TEST RAX, RAX  (48 85 C0)
        self.buf.emit(&[0x48, 0x85, 0xC0]);
        // JZ rel32 → shared exception-check stub (patched later)
        self.buf.emit(&[0x0F, 0x84]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
        if precise_exc_stub {
            self.deopt_stubs.push((patch_offset, throw_bci, 9));
        } else {
            self.exception_check_stubs
                .push((patch_offset, self.dbg_last_pc));
        }
        // Force `has_dispatch` (see the field doc): the fallible `jit_newarray`
        // helper needs the per-thread `JIT_THREAD` TLS set — both to run the
        // allocation-failure GC and to construct the OOME — which only the
        // dispatch-aware entry path (`set_jit_thread`) provides.
        self.emitted_alloc_oom_check = true;
    }

    /// Emit the single shared out-of-line stub for post-invoke exception
    /// guards. Loads the `i64::MIN` deopt sentinel into RAX and runs the
    /// standard method epilogue. All `CMP/JE` guards emitted by
    /// `emit_post_invoke_exception_check` are patched to branch here.
    pub(super) fn emit_exception_check_stub(&mut self) {
        if self.exception_check_stubs.is_empty() {
            return;
        }

        // One stub per DISTINCT throw-site bci, not one shared stub.
        //
        // The bci matters because `JitSignals::athrow_bci` is consumed by
        // `execute_jit_call` as *this* method's throw site and range-checked
        // against `[start_pc, end_pc)` of every entry in this method's own
        // exception table. Until this stub stamps it, that field still held
        // whatever the CALLEE's compiled `athrow` lowering left there -- a pc
        // in a different method, which lands inside this method's protected
        // region only by coincidence.
        //
        // A typed handler survives that coincidence often enough to look
        // healthy (it is also matched on exception class), but a catch-all
        // (`catch_type == 0`, i.e. a javac `finally`) has nothing else to
        // match on: a foreign bci outside the region silently drops it and the
        // `finally` never runs. `FinallyBalanceProbe.java` is the witness --
        // `try { n++; thrower(); } finally { n--; }` leaked one count per
        // throw under JIT and zero under `--nojit` / HotSpot.
        //
        // Grouping by bci keeps the cost at one small pad per distinct
        // fallible bytecode rather than per branch site (unrolled loop copies
        // share their original bci).
        let sites = self.exception_check_stubs.clone();
        let mut stub_by_bci: FxHashMap<usize, usize> = FxHashMap::default();
        for (patch_off, bci) in sites {
            // The stamped value is range-tested against THIS method's
            // interpreter exception table, so it must be an interpreter bci.
            // Identity on an ordinary compile; under a loop rewrite the copies
            // of one bytecode collapse onto their shared original bci, which
            // is exactly the grouping the comment above describes.
            let bci = self.orig_bci(bci);
            let stub_offset = match stub_by_bci.get(&bci) {
                Some(&off) => off,
                None => {
                    let off = self.buf.pos();
                    stub_by_bci.insert(bci, off);
                    // Stamp this method's own throw-site bci over whatever the
                    // callee left behind. The argument registers are dead here
                    // -- the method is about to return.
                    self.emit_mov_imm32_sx(ARG_REGS[0], bci as i32); // Cast: bci fits i32
                    self.emit_call_absolute(self.helpers.set_throw_bci);
                    // MOV RAX, i64::MIN - deopt sentinel so the interpreter's
                    // post-JIT path treats this as a deopt return and drains
                    // the pending exception. 48 B8 <imm64>
                    self.buf.emit(&[0x48, 0xB8]);
                    self.buf.emit(&(i64::MIN as u64).to_le_bytes()); // Cast: x86-64 immediate encoding
                    // Standard method epilogue: restore callee-saved regs and return.
                    self.emit_epilogue();
                    off
                }
            };
            let rel32 = (stub_offset as i32) - (patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
            self.buf.try_patch_i32(patch_off, rel32).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
        }
    }

    /// Emit out-of-line deoptimization stubs at the end of the method.
    ///
    /// Each stub calls `jit_uncommon_trap(vm_ptr, reason, bci)` and then
    /// returns `i64::MIN` as a sentinel to tell the interpreter that the
    /// method was deoptimized and should be re-executed in the interpreter.
    pub(super) fn emit_deopt_stubs(&mut self) {
        if self.deopt_stubs.is_empty() {
            return;
        }

        // We need one stub per (bci, reason) pair since the BCI differs.
        // However, many guards may share the same BCI — group and share stubs.
        let stubs: Vec<(usize, usize, i64)> = self.deopt_stubs.clone();

        // Map from (bci, reason) to the emitted stub offset
        let mut stub_offsets: FxHashMap<(usize, i64), usize> = FxHashMap::default();

        for &(patch_off, site_pc, reason) in &stubs {
            // `site_pc` is the EMITTER pc the guard was recorded at, and every
            // `*_box_ptr_by_bci` map below is keyed by that same emitter pc.
            // Stub SHARING stays keyed on it too: under a loop rewrite two
            // copies of one bytecode have the same original bci but may have
            // recorded different frame snapshots, and sharing a stub would
            // bake one copy's snapshot pointer into the other copy's exit.
            // Identity — and therefore byte-identical — on an ordinary
            // compile, where `site_pc` IS the bci.
            let key = (site_pc, reason);
            if let Some(&stub_off) = stub_offsets.get(&key) {
                // Reuse existing stub
                let rel32 = (stub_off as i32) - (patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
                self.buf.try_patch_i32(patch_off, rel32).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                continue;
            }

            let stub_off = self.buf.pos();
            stub_offsets.insert(key, stub_off);

            // reason 3 = divide-by-zero: direct-throw `ArithmeticException`
            // instead of `jit_uncommon_trap` (which re-runs the method from
            // entry, double-executing any side effect that preceded the trap —
            // a HotSpot divergence). `jit_throw_arithmetic()` takes no args, sets
            // the pending-arithmetic + deopt flags, and returns the `i64::MIN`
            // sentinel in RAX; the interpreter's JIT-return drain throws the
            // exception through this method's own exception table (no re-run).
            // Mirrors the bounds-check `jit_throw_aioobe` stub. Checked BEFORE the
            // frame-deopt trampoline below so reason 3 always direct-throws,
            // regardless of the deopt-osr gate.
            if reason == 3 {
                // CALL jit_throw_arithmetic (returns i64::MIN sentinel in RAX).
                self.emit_call_absolute(self.helpers.throw_arithmetic);
                // RAX already holds i64::MIN from the helper return; run the
                // standard epilogue (preserves RAX) and return to the caller.
                self.emit_epilogue();
                let rel32 = (stub_off as i32) - (patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
                self.buf.try_patch_i32(patch_off, rel32).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
                continue;
            }

            // deopt-osr Step 2 — route the BCE pilot guard (reason 2) and the
            // deopt-osr Step 8 OSR-exit trigger (reason 7) to the in-stub 3-arg
            // frame-deopt trampoline, under CRATONVM_DEOPT_REAL with a recorded
            // snapshot for this bci. Reason 2 bakes the guard snapshot
            // (`deopt_box_ptr_by_bci`); reason 7 bakes the OSR-exit map
            // (`osr_exit_box_ptr_by_bci`) — both reconstruct + resume at `bci`.
            // Gate OFF (default) ⇒ None ⇒ the uncommon-trap path below emits
            // byte-identically.
            //
            // RESTORED, with the REAL root cause fixed (2026-07-07,
            // jit-invokedynamic-groovy-regression, FOURTH pass — see the
            // known-issues doc for the full history). `fb4a333d` routed
            // reason 8 (`DEOPT_REASON_UNREACHED_CODE`, the invokedynamic
            // uncommon trap) through this precise path UNCONDITIONALLY, to
            // fix a genuine, confirmed silent-corruption bug (the old
            // imprecise "safe reject" rewound to the OSR entry bci,
            // discarding or duplicating side effects committed by
            // JIT-compiled code between OSR entry and the trap — confirmed via
            // the `AccumRepro3`/`LicmRepro`/`LicmRepro2`/`ArrRepro` standalone
            // springboot run many times over: pre-`fb4a333d`, `AccumRepro3`
            // silently and nondeterministically doubles a loop's iteration
            // count roughly 90% of the time). That routing then appeared to
            // independently regress Groovy (every
            // `GroovyBeanDefinitionReaderTests` method failing with a
            // Groovy-compiler-internal "duplicate main method" error under
            // JIT-on) — an EARLIER pass here shipped a blanket revert of just
            // this routing (reason 8 forced back to `None`, reopening the
            // original corruption) as a stopgap, which was explicitly rejected
            // as a final answer: it traded a loud, well-understood bug for a
            // quieter, already-documented one instead of fixing both.
            //
            // ACTUAL ROOT CAUSE (found on this pass, NOT a stack-soundness
            // gap): `emit_osr_exit_map_at` is shared by two call sites — the
            // true loop-header OSR-exit AND this invokedynamic trap snapshot —
            // and BOTH used to hard-code the box's `DeoptReason` as `OsrExit`.
            // `real_frame_deopt_resume_and_despeculate`'s de-speculation step
            // recovers the reason FROM THE BOX, not from the `8` baked into
            // `deopt_stubs`, so every time this "uncommon" trap actually fired
            // it was de-speculated as an ordinary recoverable `OsrExit`
            // (count-based recompile-and-retry) instead of `UnreachedCode`'s
            // "give up immediately" (`MakeNotCompilable`) policy. For Groovy's
            // `IndyInterface`-based dynamic dispatch, where this trap is
            // reached on essentially EVERY call (never actually "unreached"),
            // that meant the method stayed compiled and re-entered the trap
            // on every subsequent invocation, each time pushing a BRAND NEW
            // interpreter frame via `resume_real_ir_deopt` to redo the call
            // from scratch — confirmed via `CRATONVM_DBG_DEOPT`: a single
            // failing `simpleBean()` run showed ~2000 frame-deopt entries (all
            // mis-labeled `reason=OsrExit`), each one a fresh re-entry into
            // Groovy's own script/closure class-generation machinery, exactly
            // matching the "doCall duplicates another method" symptom (Groovy
            // observing its own generated method registered more than once).
            // Fixed at the SOURCE: `emit_osr_exit_map_at_reason` now lets each
            // call site stamp its own correct reason (`UnreachedCode` for the
            // 0xba trap, `OsrExit` for the loop-header case), so
            // `recommend_action` finally sees `UnreachedCode` and gives up on
            // first occurrence, as `fb4a333d` always intended. This, together
            // with a second, independent, unconditionally-correct fix
            // (`getstatic`'s codegen never marked a reference-typed static
            // field's pushed value as an oop, unlike `getfield`'s inline arms —
            // see the `mark_top_as_oop()` calls added at both `0xb2` codegen
            // sites — which was the actual cause of `LicmRepro`/`LicmRepro2`/
            // `ArrRepro`'s `NullPointerException`), is what makes it safe to
            // restore reason 8's unconditional precise-resume routing here.
            //
            // ADDITIONALLY (same day, concurrent branch fix/hib-temporal-
            // placeholder-dup-20260707): a SECOND, independent unsoundness in
            // the same resume machinery — the stashed `ReconstructedFrame`
            // carried NO method identity (`method_key` was empty), so a trap
            // in a NESTED compiled callee (sentinel bubbling up through its
            // compiled callers' epilogue bails) could be resumed by an outer
            // consumer as if it were the OUTER method's frame: foreign
            // locals/stack/bci, arbitrary misexecution. Closed by (a) baking
            // `self.method_key` into every snapshot
            // (`build_and_record_deopt_point`), (b) identity checks at every
            // VM resume sink, (c) `try_resume_trapped_callee`
            // (vm/src/jit/helpers.rs): dispatch helpers resolve a trapped
            // callee precisely at the call site, and (d) `has_indy_trap`
            // publication gates so machine code never direct-calls an
            // indy-trap artifact (no helper there could resolve it). Repro:
            // scratch-min/IndyReplay.java (nested shape: 30000/30000 calls
            // corrupted side effects before these fixes, 0 after).
            // The value HANDED TO THE RUNTIME, as opposed to the emitter pc
            // used for keying above: `jit_uncommon_trap`'s 3rd argument is an
            // interpreter bci and the interpreter resumes at it. Identity
            // unless this compile is emitting rewritten bytecode.
            let bci = self.orig_bci(site_pc);
            let frame_box_ptr = if crate::deopt_real_enabled() || matches!(reason, 8 | 9 | 10) {
                match reason {
                    2 => self.deopt_box_ptr_by_bci.get(&site_pc).copied(),
                    // Step 6: String-intrinsic and call-site type-check guards
                    // (ReceiverTypeChanged). The same `deopt_box_ptr_by_bci` map
                    // is used; a snapshot was recorded by `snapshot_pre_intrinsic_call`
                    // immediately after the pre-call flush (before any arg pops).
                    6 => self.deopt_box_ptr_by_bci.get(&site_pc).copied(),
                    7 => self.osr_exit_box_ptr_by_bci.get(&site_pc).copied(),
                    // Reason 8 (UnreachedCode / invokedynamic trap): unconditional,
                    // NOT gated behind `deopt_real_enabled()` — see the doc above.
                    8 => self.osr_exit_box_ptr_by_bci.get(&site_pc).copied(),
                    // Reason 9 is a pending Java exception in a method whose
                    // handler reads non-parameter locals. Its snapshot is keyed
                    // on the THROWING invoke's own bci (not a resume point — see
                    // `emit_post_invoke_exception_check`) and lives in its own
                    // map, because it is consumed by the interpreter's exception
                    // route rather than by any resume sink.
                    9 | 10 => self.exc_frame_box_ptr_by_bci.get(&site_pc).copied(),
                    _ => None,
                }
            } else {
                None
            };
            // Fail closed if the frame did not reserve the region this path is
            // about to spill 32 registers into. `Compiler::new` reserves it on
            // the same condition that selects this path, so this is unreachable
            // — and it is checked anyway because the two conditions live in
            // different files and drifted apart once already, with the stub
            // writing over the caller's return address (see the `deopt_regs_size`
            // comment in `x64.rs`). Bailing the compile costs one interpreted
            // method; the alternative cost a corrupted stack.
            if frame_box_ptr.is_some() && self.deopt_regs_base == 0 {
                if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITC").is_some() {
                    eprintln!(
                        "[cratonvm-jitc] compile-bail deopt-stub-without-saved-regs \
                         reason={reason} site_pc={site_pc}"
                    );
                }
                self.buf
                    .mark_codegen_unencodable("deopt-stub-without-saved-regs");
                return;
            }
            if let Some(box_ptr) = frame_box_ptr {
                let base = self.deopt_regs_base;
                // deopt-osr Step 9 follow-up (a): allocate (once) the artifact's
                // retained epoch guard and bake it as the 4th arg. Leaked so its
                // address is stable for the process lifetime — even under
                // CRATONVM_JIT_FREE_CODE=1, where the artifact (and its deopt
                // boxes) may be freed, this guard survives so `x64_deopt_entry`
                // can read the live/creation epochs WITHOUT touching the box. The
                // VM stamps it (creation epoch + live-epoch cell) at install.
                if self.deopt_epoch_guard.is_null() {
                    let g = Box::new(crate::deopt::DeoptEpochGuard::new());
                    // LEAK(intentional): retained for the process lifetime; baked
                    // by raw pointer into the stub and into CompiledMethod.
                    self.deopt_epoch_guard =
                        Box::into_raw(g) as *const crate::deopt::DeoptEpochGuard;
                }
                let guard_ptr = self.deopt_epoch_guard;
                // 1) Spill all 16 GPRs (RAX=0..R15=15) into the SavedRegisters
                //    region FIRST, before any arg-setup clobbers a register: the
                //    guard `JB` reaches here with every GPR still holding its
                //    trapping-instant value. gpr[r] -> [rbp - (base - r*8)], so
                //    gpr[0]=RAX is the deepest slot and ascends with r.
                for r in 0u8..16 {
                    // Cast: value to i32 (encoding immediate/displacement)
                    self.emit_store_local(base - (r as i32) * 8, r);
                }
                // 1b) Spill all 16 XMM registers (low 64 bits via MOVQ) into the
                //     XMM half of the region — `xmm[n] -> [rbp - (base - 128 -
                //     n*8)]`, matching the `#[repr(C)]` field order
                //     (gpr[16] then xmm[16]). This captures any `float`/`double`
                //     the FP value tier kept in an XMM live across the guard, so
                //     `XmmFloat(n)`/`XmmDouble(n)` resolve in `x64_deopt_entry`.
                //     The GPR spill above does not touch XMMs, so each still holds
                //     its trapping-instant value. Always-spill-16 (like the GPRs):
                //     reading an unused XMM is harmless and keeps the stub simple.
                for n in 0u8..16 {
                    // Cast: value to i32 (encoding immediate/displacement)
                    self.emit_movq_mem_rbp_from_xmm(base - 128 - (n as i32) * 8, n);
                }
                if reason == 10 {
                    // A locally-detected putfield null has no helper return
                    // value to carry its exception. Publish it after saving
                    // the trapping registers, before materializing the
                    // precise exceptional frame.
                    #[cfg(target_os = "windows")]
                    self.buf.emit_byte(0xB9); // MOV ECX, imm32
                    #[cfg(not(target_os = "windows"))]
                    self.buf.emit_byte(0xBF); // MOV EDI, imm32
                    self.buf.emit(&(npe_action::NONE as u32).to_le_bytes());
                    self.emit_call_absolute(self.helpers.jit_npe_with_action);
                }
                // 2) Args (extern "C"): arg0 = &DeoptimizationPoint (baked imm64),
                //    arg1 = rbp (live, never clobbered until the epilogue),
                //    arg2 = &SavedRegisters = LEA [rbp - base] = &gpr[0],
                //    arg3 = &DeoptEpochGuard (baked imm64; deopt-osr Step 9 fu-a).
                #[cfg(target_os = "windows")]
                {
                    // Cast: non-negative index/count to usize
                    self.emit_mov_imm64_full(RCX, box_ptr as usize as i64);
                    self.emit_mov_reg_reg(RDX, RBP);
                    self.emit_lea_frame_slot(R8, base);
                    self.emit_mov_imm64_full(R9, guard_ptr as usize as i64);
                }
                #[cfg(not(target_os = "windows"))]
                {
                    // Cast: non-negative index/count to usize
                    self.emit_mov_imm64_full(RDI, box_ptr as usize as i64);
                    self.emit_mov_reg_reg(RSI, RBP);
                    self.emit_lea_frame_slot(RDX, base);
                    self.emit_mov_imm64_full(RCX, guard_ptr as usize as i64);
                }
                // 3) CALL the jit-crate 3-arg entry BEFORE the epilogue (rbp + the
                //    spill region are still live; reconstruction reads gpr[r]
                //    synchronously inside the entry before the epilogue's
                //    callee-saved restore overwrites the live registers). The
                //    entry stashes LAST_DEOPT and returns i64::MIN in RAX — mirrors
                //    the IR path's `emit_deopt_stub`. `emit_call_absolute`'s
                //    imm64-via-RAX fallback clobbers RAX (already spilled, dead)
                //    and leaves the 3 arg registers intact.
                // Cast: fn pointer to usize helper address
                self.emit_call_absolute(crate::deopt::x64_deopt_entry as *const () as usize);
                // 4) Force the deopt sentinel (the entry returns it; explicit for
                //    parity with the uncommon-trap stub) + epilogue.
                self.rex_w();
                self.buf.emit_byte(0xB8); // MOV RAX, imm64
                                          // Cast: non-negative value to u64
                self.buf.emit(&(i64::MIN as u64).to_le_bytes());
                self.emit_epilogue();
            } else {
                // Set up args for jit_uncommon_trap(vm_ptr: i64, reason: i64, bci: i64)
                // vm_ptr is in the heap_local (frame slot) — load it first
                #[cfg(target_os = "windows")]
                {
                    // Windows x64: arg1=RCX, arg2=RDX, arg3=R8
                    // Load vm_ptr from heap_local_offset into RCX
                    self.emit_load_local(RCX, self.heap_local_offset);
                    // MOV RDX, reason (immediate)
                    self.rex_w();
                    self.buf.emit_byte(0xB8 + RDX as u8); // MOV r64, imm64 // Cast: x86-64 register encoding
                    self.buf.emit(&(reason as u64).to_le_bytes()); // Cast: x86-64 immediate encoding
                                                                   // MOV R8, bci (immediate)
                    self.buf.emit(&[0x49, 0xB8 + (R8 as u8 - 8)]); // REX.WB + MOV r64, imm64 // Cast: x86-64 register encoding
                    self.buf.emit(&(bci as u64).to_le_bytes()); // Cast: x86-64 immediate encoding
                }
                #[cfg(not(target_os = "windows"))]
                {
                    // SysV: arg1=RDI, arg2=RSI, arg3=RDX
                    // Load vm_ptr from heap_local_offset into RDI
                    self.emit_load_local(RDI, self.heap_local_offset);
                    // MOV RSI, reason (immediate)
                    self.rex_w();
                    self.buf.emit_byte(0xB8 + RSI as u8); // Cast: x86-64 register encoding
                    self.buf.emit(&(reason as u64).to_le_bytes()); // Cast: x86-64 immediate encoding
                                                                   // MOV RDX, bci (immediate)
                    self.rex_w();
                    self.buf.emit_byte(0xB8 + RDX as u8); // Cast: x86-64 register encoding
                    self.buf.emit(&(bci as u64).to_le_bytes()); // Cast: x86-64 immediate encoding
                }

                // CALL jit_uncommon_trap
                self.emit_call_absolute(self.helpers.uncommon_trap);

                // Return i64::MIN as deopt sentinel
                // MOV RAX, i64::MIN
                self.rex_w();
                self.buf.emit_byte(0xB8); // MOV RAX, imm64
                self.buf.emit(&(i64::MIN as u64).to_le_bytes()); // Cast: x86-64 immediate encoding

                // Epilogue: restore callee-saved regs and return
                // This mirrors the standard method epilogue
                self.emit_epilogue();
            } // end else (uncommon-trap path)

            // Patch the branch to point here
            let rel32 = (stub_off as i32) - (patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
            self.buf.try_patch_i32(patch_off, rel32).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
        }
    }
}

#[cfg(test)]
mod spill_region_contract {
    //! The frame reservation and the stub's spill must agree.
    //!
    //! They live in different files and drifted apart once, with the stub
    //! writing 32 registers over the caller's saved `rbp` and return address.
    //! `probes/IndyDeoptProbe.java` is the executable reproducer; this is the
    //! same contract at unit scale, checkable in the configuration that
    //! actually broke.

    use crate::x64::deopt_spill_region_reserved as reserved;

    /// The reasons `emit_deopt_stubs` takes the REGISTER-SPILLING path for when
    /// `deopt_real` is off. Transcribed from its
    /// `crate::deopt_real_enabled() || matches!(reason, 8 | 9 | 10)`.
    const SPILLS_WITHOUT_DEOPT_REAL: [i64; 3] = [8, 9, 10];

    /// Which compile-time property covers each of those reasons.
    ///
    /// Reason 8 is the unconditional `invokedynamic` trap; 9 and 10 are the
    /// precise-exception-frame stubs. A reason with no covering property is a
    /// stub that can spill into a region the frame never reserved.
    fn covered_by(reason: i64) -> Option<&'static str> {
        match reason {
            8 => Some("has_indy_sites"),
            9 | 10 => Some("precise_exception_frames"),
            _ => None,
        }
    }

    #[test]
    fn every_stub_that_spills_without_deopt_real_has_a_frame_reservation() {
        for reason in SPILLS_WITHOUT_DEOPT_REAL {
            let property = covered_by(reason).unwrap_or_else(|| {
                panic!(
                    "reason {reason} spills 32 registers with `deopt_real` off and \
                     nothing reserves the region it spills into"
                )
            });
            // …and the reservation predicate really does answer `true` for that
            // property alone, with `deopt_real` OFF. This is the assertion the
            // bug failed: `has_indy_sites` was not a term in it at all.
            let (pef, indy) = (property == "precise_exception_frames", property == "has_indy_sites");
            assert!(
                reserved(false, pef, indy),
                "reason {reason}: {property} does not reserve the spill region"
            );
        }
    }

    /// The default path is untouched: nothing to spill, nothing reserved.
    #[test]
    fn a_method_with_nothing_to_spill_reserves_nothing() {
        assert!(!reserved(false, false, false));
        // …and `deopt_real` on its own still reserves, which is what makes the
        // production frame byte-identical to what it was.
        assert!(reserved(true, false, false));
    }

    /// The edit that would trip this suite: dropping `has_indy_sites` from
    /// `deopt_spill_region_reserved`, or adding a reason to the stub's
    /// `matches!(reason, ...)` without a covering property here.
    #[test]
    fn the_contract_is_stated_in_both_directions() {
        assert!(reserved(false, false, true), "an indy method must reserve");
        assert!(reserved(false, true, false), "a precise-frame method must reserve");
        for reason in SPILLS_WITHOUT_DEOPT_REAL {
            assert!(covered_by(reason).is_some(), "reason {reason} uncovered");
        }
        assert!(covered_by(2).is_none(), "reason 2 spills only under deopt_real");
    }
}
