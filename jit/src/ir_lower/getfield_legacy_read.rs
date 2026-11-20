#[allow(unused_imports)]
use super::*;

impl<'a> Lowerer<'a> {
    /// The legacy-layout read of an inline field read: the uniform 16-byte
    /// `Value` cell at `legacy_cell_off`, RAX = the receiver on entry and the
    /// value on exit; a reference cell whose discriminant is not an object
    /// takes the checked helper (`slow`). Extracted unchanged from
    /// [`Self::emit_inline_compact_getfield`] (JIT round 23 wave 9, lane
    /// W9-L7) so the inline arm and its deferred cold tail
    /// ([`Self::emit_deferred_getfield_tails`]) emit the same bytes.
    pub(in crate::ir_lower) fn emit_getfield_legacy_read(
        &mut self,
        ref_node: bool,
        type_tag: u8,
        legacy_cell_off: i32,
        slow: &mut Vec<usize>,
        dst: u8,
        receiver: u8,
    ) {
        if !ref_node {
            let (payload, payload_tag) = match type_tag {
                b'J' | b'D' => (FIELD_CELL_PAYLOAD64_OFFSET, type_tag),
                // Legacy int-category values occupy a full Value::Int payload.
                _ => (
                    FIELD_CELL_PAYLOAD32_OFFSET,
                    if type_tag == b'F' { b'F' } else { b'I' },
                ),
            };
            self.emit_compact_prim_field_load_to(
                payload_tag,
                legacy_cell_off + payload as i32,
                dst,
                receiver,
            );
            return;
        }
        // The cell's DISCRIMINANT decides whether that payload is a
        // pointer. Reading the payload without asking was the Tomcat/Derby
        // SIGSEGV of 2026-08-23: `SQLChar.rawData` is declared `[C`, its
        // cell held `Value::Int(1)`, and this arm loaded the 8-byte payload
        // word — 1 — and handed it on as a reference. The `arraylength`
        // three instructions later is `MOV r32,[RAX+4]`, so the process
        // died at `addr=0x5`, deterministically: three crashes carried
        // byte-identical registers.
        //
        // The helper this arm exists to skip has always looked at the
        // variant (`read_value_atomic` then `match val`), which is why the
        // crash was invisible in the helper path and why the fix belongs
        // here. Deferring to it — rather than degrading to null inline —
        // keeps ONE place deciding what a punned slot means, and that place
        // counts it (`getfield reference loads that contained a primitive
        // slot`).
        //
        // Cost is one compare and one not-taken branch on the legacy
        // reference path; the compact arm above is untouched, and it is the
        // one this whole inline sequence was written for.
        if receiver == RAX {
            self.buf.emit(&[0x83, 0xB8]); // CMP DWORD [RAX + disp32], imm8
            self.buf.emit(
                &(legacy_cell_off + cratonvm_types::FIELD_CELL_TAG_OFFSET as i32).to_le_bytes(),
            );
            self.buf
                .emit_byte(cratonvm_types::FIELD_CELL_TAG_OBJECT as u8);
        } else {
            use crate::x64::isel::{Mem, Op as MachineOp, Operand, Req, Ty};
            self.emit_field_instruction(Req::new(
                MachineOp::Cmp,
                Ty::I32,
                Operand::Mem(Mem::base_disp32(
                    receiver,
                    i64::from(legacy_cell_off + cratonvm_types::FIELD_CELL_TAG_OFFSET as i32),
                )),
                Operand::Imm(cratonvm_types::FIELD_CELL_TAG_OBJECT as i64),
            ));
        }
        slow.push(self.emit_jcc_rel32(0x85)); // JNE → the checked helper

        // A reference descriptor always reads the cell's 64-bit pointer
        // payload — never the 32-bit MOVSXD for integer cells, which would
        // sign-extend half a pointer into a bogus non-null receiver.
        if receiver == RAX && dst == RAX {
            self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
            self.buf
                .emit(&(legacy_cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32).to_le_bytes());
        } else {
            use crate::x64::isel::{Mem, Op as MachineOp, Operand, Req, Ty};
            self.emit_field_instruction(Req::new(
                MachineOp::Mov,
                Ty::I64,
                Operand::Gpr(dst),
                Operand::Mem(Mem::base_disp32(
                    receiver,
                    i64::from(legacy_cell_off + FIELD_CELL_PAYLOAD64_OFFSET as i32),
                )),
            ));
        }
    }

    /// Guarded inline read of a compact instance field, with the checked
    /// `jit_getfield` helper as the slow path. Returns `false` when the site is
    /// not eligible, leaving the caller's helper-only lowering in place.
    ///
    /// Why this exists: routing every field read through the helper costs a
    /// JIT-boundary note, an `is_object_address` region walk and a 16-byte
    /// atomic cell read. When that hardening first landed on the single-pass
    /// backend it cost 4.7x on bintrees-16, which is why that backend grew the
    /// guarded inline path (`guarded_inline_getfield_enabled`). The IR tier
    /// never got one, and it is the measured reason a forced-C2 `bt18` ran
    /// **1.85x slower** than the single-pass body it replaced (3614-3762 ms vs
    /// 6436-7013 ms, five interleaved reps) once reference field reads made
    /// real methods IR-eligible. An optimizing tier that reads fields more
    /// expensively than the baseline tier cannot be worth selecting.
    ///
    /// The shape mirrors `x64.rs`'s arm exactly, and deliberately keeps the
    /// property that stops the stale-receiver SIGSEGV: never dereference a
    /// receiver that is not null-free, 8-aligned and inside a published GC
    /// region. Everything else — null, unaligned, out-of-heap, or a width this
    /// arm does not emit — branches to the helper, whose NPE / `i64::MIN`
    /// semantics are unchanged.
    ///
    /// **The legacy-layout receiver reads inline too, since 2026-08-18.** It
    /// used to take the helper — "the one simplification against the
    /// single-pass version" — and that simplification turned out to be 100% of
    /// this helper's calls on the Generational collector. `init_object_header`,
    /// the TLAB fast path that serves ~99% of allocations for both the
    /// interpreter and `jit_new_object`, writes `array_length = 0` and no
    /// `GC_FLAG_COMPACT` unconditionally: it never consults
    /// `plan_object_alloc`, so a class with a perfectly good registered compact
    /// layout is still allocated legacy. An arm that inlines only compact
    /// receivers therefore inlines almost nothing. See
    /// every-jit-getfield-takes-the-helper-FIXED-20260820.md.
    ///
    /// The legacy read is the uniform 16-byte `Value` cell at
    /// `HEADER_SIZE + field_index * SLOT_SIZE`, transcribed from the
    /// single-pass arm's own legacy branch so the two cannot disagree about
    /// payload offsets or sign-extension.
    pub(in crate::ir_lower) fn emit_inline_compact_getfield(
        &mut self,
        node_pc: Option<usize>,
        node_ty: IrType,
        base: NodeId,
        field_index: i64,
        slot: i32,
        volatile_access: bool,
    ) -> bool {
        if self.getfield == 0 {
            crate::metrics::note_ir_getfield_decline(0);
            return false;
        }
        let Some(pc) = node_pc else {
            crate::metrics::note_ir_getfield_decline(1);
            return false;
        };
        // JIT r20 wave 18 (lane xLib, THROW20-4): a spliced
        // `Reference.referent` read (`Graph::referent_load_pcs`) always takes
        // the checked helper, whose slot-0 arm logs the referent through the
        // SATB keep-alive; no inline arm here has one (SBR4 satb). Checked
        // before every row and arm below, the legacy-only one included.
        //
        // JIT round 21 (lane tl3, TL221-1, `CRATONVM_JIT_IR_REFERENT_GATED_INLINE`,
        // default ON, `=0` restores the unconditional decline): the read is
        // inline behind the collector's SATB pre-gate byte instead
        // ([`referent_gated_inline_admitted`]); an armed byte takes the helper.
        //
        // JIT round 21 (lane g1vhsoft): the gate is the REFERENCE load's at
        // such a pc. The `WeakReference` / `SoftReference.get()` expansion
        // (`IrBuilder::try_reference_get_intrinsic`) also reads the `long`
        // `timestamp` there, an ordinary field read; a spliced
        // `Reference.get` pc holds the referent load alone, as before.
        let referent_gate = node_ty == IrType::Ref && self.graph.referent_load_pcs.contains(&pc);
        if referent_gate && !referent_gated_inline_admitted(node_ty, self.satb_pre_gate) {
            crate::metrics::note_ir_getfield_decline(2);
            return false;
        }
        // j27 wave 5 (lane REFS): on a heap whose ZGC referent read gate may
        // arm, no referent read is inline (the gate is the helper's alone).
        // The VM query runs only for a referent read.
        let compare_only = self.getfield_no_keepalive_bit(pc) != 0;
        let gate_may_arm = (referent_gate || compare_only) && self.direct_helpers.referent_gate_may_arm();
        // Under a gate that may arm, the value a compare-only read is compared
        // against; a null one takes the helper below.
        let gated_compare_operand = (gate_may_arm && compare_only && !referent_gate)
            .then(|| self.compare_only_read_operand())
            .flatten();
        if (referent_gate || compare_only)
            && referent_read_owes_the_checked_helper(
                gate_may_arm,
                referent_gate,
                compare_only,
                gated_compare_operand.is_some(),
            )
        {
            crate::metrics::note_ir_getfield_decline(2);
            return false;
        }
        // Keyed by `(pc, is_reference)` and not by `pc` alone: the IR
        // String-access expansion (`ir.rs::try_string_access_intrinsic`)
        // emits TWO `Op::Load`s at one `invokevirtual` pc — `coder` (Int)
        // and `value` (Ref) — so a pc-only key could describe at most one
        // of them and the other would fall to the checked helper for want
        // of a row rather than for any reason about the field. An ordinary
        // `getfield` pc carries exactly one row, under its own field's
        // reference-ness, so nothing about that case changes.
        // nio21 wave 18 (lane L3): a load an optimizer pass placed at a pc
        // that is not its own field instruction (`Graph::synthetic_field_access_pcs`)
        // never takes that pc's row: the row names the pc's OWN field, and
        // nothing in it says which. It reads through the field-index arms
        // (the legacy-only inline read, or the checked helper).
        let row = if referent_gate {
            // JIT round 21 (lane tl3): a referent site has no row by design
            // (`REFERENT_SITE_TAG`); the legacy-only arm reads it, a compact
            // receiver takes the helper.
            //
            // JIT round 21 (lane g1vhsoft, `CRATONVM_JIT_IR_REFERENCE_GET_INLINE`):
            // unless the builder named this exact load's row itself
            // (`Graph::field_rows`, the `Reference.get` expansion's, from the
            // VM's compact layout of the receiver's exact class). The compact
            // arm below then keeps the same keep-alive gate after its read.
            self.cur_def.and_then(|d| {
                self.graph
                    .field_rows
                    .get(&d)
                    .copied()
                    .filter(|&(_, is_ref, tag)| is_ref && tag == b'L')
                    .filter(|_| {
                        self.graph.nodes.get(d as usize).is_some_and(|n| {
                            matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(pc)
                        })
                    })
            })
        } else if self.graph.synthetic_field_access_pcs.contains(&pc) {
            // nio21 wave 20 (lane L3): unless the pass that placed THIS load
            // named its exact field's row (`Graph::field_rows`).
            self.cur_def.and_then(|d| {
                let is_this_load = self
                    .graph
                    .nodes
                    .get(d as usize)
                    .is_some_and(|n| matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(pc));
                if !is_this_load {
                    return None;
                }
                self.graph
                    .field_rows
                    .get(&d)
                    .copied()
                    .filter(|&(_, is_ref, _)| is_ref == (node_ty == IrType::Ref))
                    // `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: or the row
                    // of the `getfield` of the same field it answers for.
                    .or_else(|| {
                        let src = *self.graph.field_row_pcs.get(&d)?;
                        if self.graph.synthetic_field_access_pcs.contains(&src) {
                            return None;
                        }
                        self.compact_fields
                            .get(&(src, node_ty == IrType::Ref))
                            .copied()
                    })
            })
        } else {
            self.compact_fields
                .get(&(pc, node_ty == IrType::Ref))
                .copied()
        };
        // Round 16 (lane fieldguard, FG16-7, `CRATONVM_JIT_IR_LEGACY_FIELD_INLINE`):
        // a site with NO compact row still reads inline, through the legacy
        // arm alone -- see `legacy_only_field_tag` for why that is sound.
        let legacy_only = row.is_none();
        let Some((c_off, c_is_ref, type_tag)) = row.or_else(|| {
            legacy_only.then(|| (0, node_ty == IrType::Ref, legacy_only_field_tag(node_ty)))
        }) else {
            crate::metrics::note_ir_getfield_decline(2);
            return false;
        };
        // The legacy arm's bounds check bakes the index as an imm32.
        let Ok(field_index_imm) = i32::try_from(field_index) else {
            crate::metrics::note_ir_getfield_decline(2);
            return false;
        };
        // zt t23/zb6 (stage 2d-ii): under a COLORED-store ZGC load barrier a
        // compact REFERENCE load reads inline and decodes
        // (`x64::zgc_colored_inline_field_load_base`); every other shape of
        // an armed (or narrow-oop) process keeps the checked helper, as
        // before. `None` whenever the gate below is open, so an unarmed
        // compile emits what it always did.
        let raw_mode = crate::x64::inline_getfield_enabled();
        // zt t32 (successor page section 7, `CRATONVM_ZGC_INLINE_PRIMITIVE_FIELDS`):
        // under an armed barrier a PRIMITIVE load of a receiver typed `Ref`
        // still reads inline -- the trusted-oop arm below (a null test, no
        // read-bounds table), no barrier owed. Every other armed shape keeps
        // the helper.
        let armed_primitive = crate::x64::zgc_inline_field_loads_gated()
            && node_ty != IrType::Ref
            && !volatile_access
            && !raw_mode
            && crate::x64::guarded_inline_getfield_enabled()
            && crate::x64::zgc_armed_inline_primitive_fields_enabled()
            && crate::x64::trusted_oop_receiver_getfield_enabled()
            && self.graph.nodes[base as usize].ty == IrType::Ref;
        let colored_base = if crate::x64::zgc_inline_field_loads_gated() && !armed_primitive {
            match crate::x64::zgc_colored_inline_field_load_base() {
                // A legacy-only site too (lambda-proxy captures): its legacy
                // read applies the same barrier to the cell's payload word.
                Some(b)
                    if node_ty == IrType::Ref
                        // refs2: volatile ordering does not make a compare-only load keep alive.
                        && (!volatile_access || (compare_only && !referent_gate)) =>
                {
                    Some(b)
                }
                // JIT round 25 (item 4 wave 2): a PRIMITIVE read needs no load
                // barrier -- a primitive cell is never colored -- so under an
                // armed ZGC barrier (narrow oops off) it reads inline through
                // the trusted-oop arm, which never consults the cleared read
                // table. See `prim_field_read_survives_the_read_gate`.
                _ if self.prim_field_read_survives_the_read_gate(node_ty, base) => None,
                _ => {
                    crate::metrics::note_ir_getfield_decline(3);
                    return false;
                }
            }
        } else {
            None
        };
        let guarded = crate::x64::guarded_inline_getfield_enabled()
            && (self.read_bounds_addr != 0 || armed_primitive);
        if !raw_mode && !guarded {
            crate::metrics::note_ir_getfield_decline(4);
            return false;
        }
        // zt t23/zb6: the colored arm's receiver rule is the trusted-oop one
        // (the base node typed `Ref`; a null test, nothing else). The guarded
        // containment and `emit_read_bounds_live_check` both read
        // `JIT_READ_BOUNDS`, which arming clears, so either would send every
        // receiver to the helper; the raw opt-in has no slow tail for the
        // color flip. Refused shapes keep the helper.
        if colored_base.is_some()
            && (raw_mode
                || !crate::x64::trusted_oop_receiver_getfield_enabled()
                || self.graph.nodes[base as usize].ty != IrType::Ref)
        {
            crate::metrics::note_ir_getfield_decline(3);
            return false;
        }
        // Trusted-oop receiver: null check only, no containment.
        //
        // The single-pass backend has had this since
        // `emit_trusted_oop_receiver_check` landed — "a value whose
        // operand-stack type is already proven to be an oop cannot be an
        // unaligned integer or an arbitrary out-of-heap address without an
        // earlier JIT/GC correctness failure, so repeating the six arena-bound
        // comparisons at every field access is redundant". The IR tier never
        // got it, and that is why ZGC and G1 — which deliberately publish NO
        // region bounds — fail the containment clause on 100% of receivers
        // here while the single-pass arm sails through on its proven oops.
        //
        // The IR's proof is its own type lattice: `Op::Load`'s base node is
        // typed `IrType::Ref`. That is at least as strong as the single-pass
        // `stack_oop_marks` argument this reuses.
        //
        // **Restricted to PRIMITIVE fields, deliberately.** Dropping
        // containment for a REFERENCE load would inline-read a word that, under
        // ZGC, may be `Z_COLORED_TAG | colour | offset` rather than a pointer —
        // the un-barriered colored word `heap.rs::read_prim_element` panics on
        // by design, and `zgc-jit-load-barrier.md` (risk J1) rates the silent
        // version worse than a SIGSEGV. Primitives need neither a load barrier
        // nor narrow-oop decoding, so they are the whole safe set.
        //
        // Note what this does NOT do: it does not publish `JIT_REGION_BOUNDS`
        // on a non-publishing collector. That table's emptiness is load-bearing
        // — per `g1-audit.md` §8.1 (G1-2) it is the interlock that keeps
        // every inline reference-STORE fast path unreachable under G1/ZGC, so a
        // JNI-pinned CSet-excluded region cannot lose its remembered-set edge.
        // Filling it to speed up loads would silently re-enable those stores.
        // `node_ty != Ref` is `!ref_node`, computed here because `ref_node`
        // is not bound until the descriptor-agreement check below — which
        // still runs, and still refuses the site, before anything is emitted.
        let trusted_oop_receiver = node_ty != IrType::Ref
            && !raw_mode
            && crate::x64::trusted_oop_receiver_getfield_enabled()
            && self.graph.nodes[base as usize].ty == IrType::Ref;
        // Round 16 (lane fieldguard, FG16-6, `CRATONVM_JIT_IR_TRUSTED_REF_GETFIELD`,
        // default OFF): the same receiver proof for a REFERENCE load, with the
        // one thing the containment compares were ALSO doing for it kept. See
        // `emit_read_bounds_live_check`: the six compares asked two questions
        // at once -- "is the receiver mapped" (answered by the `Ref` type, as
        // for a primitive) and "do reference slots hold plain pointers right
        // now" (answered only by the table not being cleared). This form asks
        // the second one alone.
        // JIT round 21 (lane tlget, `CRATONVM_JIT_IR_THREAD_LOCAL_TRUSTED_LOADS`):
        // or a `ThreadLocal` expansion load the builder listed
        // (`Graph::tl_trusted_loads`), the node itself at this pc.
        let tl_trusted = self.cur_def.is_some_and(|d| {
            self.graph.tl_trusted_loads.contains(&d)
                && self
                    .graph
                    .nodes
                    .get(d as usize)
                    .is_some_and(|n| matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(pc))
        });
        let trusted_ref_receiver = node_ty == IrType::Ref
            && guarded
            && !raw_mode
            && (true || tl_trusted)
            && crate::x64::trusted_oop_receiver_getfield_enabled()
            && self.graph.nodes[base as usize].ty == IrType::Ref;
        // The node type and the resolved descriptor must agree. They can only
        // disagree through a resolver that fabricated a compact slot — the
        // WildFly Host Controller SIGSEGV — and the consequence of trusting it
        // here would be a 32-bit sign-extended load of half a pointer.
        let ref_node = node_ty == IrType::Ref;
        let ref_tag = matches!(type_tag, b'L' | b'[');
        if ref_node != ref_tag || ref_tag != c_is_ref {
            crate::metrics::note_ir_getfield_decline(5);
            return false;
        }
        // Width agreement, per descriptor. `J`/`D`/`F` were refused outright
        // until 2026-08-17; measurement showed that refusal was ~88% of every
        // `jit_getfield` call in a field-dense run — four sites falling back to
        // an UNGUARDED helper CALL, which is why the count was identical on
        // ZGC, Generational and G1. A `long` field on a hot path
        // (BouncyCastle's `GeneralDigest.byteCount`) is not an exotic shape.
        //
        // Each is admitted only when the IR node's own type agrees with the
        // resolved descriptor — the same defence the ref/non-ref check above
        // applies, and for the same reason: a resolver that fabricated a
        // compact slot must not steer a load width.
        let width_ok = match type_tag {
            b'I' | b'Z' | b'B' | b'C' | b'S' => node_ty == IrType::Int,
            b'J' => node_ty == IrType::Long,
            b'D' => node_ty == IrType::Double,
            b'F' => node_ty == IrType::Float,
            _ => false,
        };
        if !ref_node && !width_ok {
            crate::metrics::note_ir_getfield_decline(6);
            return false;
        }

        let cell_off = c_off as i32; // absolute compact displacement
        let no_keepalive = self
            .cur_def
            .is_some_and(|d| self.graph.no_keepalive_referent_loads.contains(&d));
        if !volatile_access && !referent_gate && !no_keepalive {
            if let Some(prev_id) = self.block_cse_find_field_load(
                base,
                field_index as u32,
                node_ty,
                cell_off,
                type_tag,
            ) {
                let direct_result = if ref_node {
                    (colored_base.is_none() && self.ref_residency_on)
                        .then(|| self.reference_result_gpr(slot))
                        .flatten()
                } else {
                    self.integer_result_gpr(node_ty, slot)
                };
                let dst = direct_result.unwrap_or(RAX);
                self.gp_load_value(dst, prev_id);
                if let Some(dst) = direct_result {
                    self.publish_integer_result(dst, slot);
                    if ref_node {
                        self.ref_def_home_stores += 1;
                    }
                } else {
                    self.store_rax(slot);
                }
                if ref_node && self.proven_fields_on {
                    self.coestablish_loaded_reference(pc);
                }
                self.block_cse_record_field_load(
                    base,
                    field_index as u32,
                    node_ty,
                    cell_off,
                    type_tag,
                    false,
                );
                return true;
            }
        }
        let mut slow: Vec<usize> = Vec::new();
        // `refersTo(null)` under an armed gate is the gate's question: a null
        // operand takes the helper (`referent_read_owes_the_checked_helper`),
        // tested in the arm below, never the proven form (it has no helper).
        let null_tested_operand = gated_compare_operand.filter(|&o| !self.node_never_null(o));
        // Past every decline: this site is now certainly emitting. Census the
        // guards it is about to repeat (see `block_inline_field_sites`).
        if self.block_inline_field_sites == 0 {
            IR_FIELD_SITE_FIRST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            IR_FIELD_SITE_LATER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.block_inline_field_sites += 1;
        // JIT round 25 (item 4, `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS`):
        // A dominating compact-receiver proof removes repeated receiver
        // dispatch. Reference fields still perform their per-read plain-slot
        // gate or colored load barrier (`emit_proven_receiver_getfield`).
        //
        // j27 wave 1 (lane TYPECHK; GC correctness, no switch): never a
        // `Reference.referent` read that hands the referent out
        // (`referent_gate`, a row named by the `Reference.get` expansion or
        // the spliced body). The proven form has no SATB pre-gate test after
        // its read, so during a concurrent mark it handed compiled code a
        // referent the keep-alive never logged -- one the cycle may then
        // clear and free while the frame still holds it. The arm below keeps
        // that test (and takes the helper when it is armed).
        let pads_before = self.deopt_pads.len();
        if self.proven_fields_on && !referent_gate && null_tested_operand.is_none() {
            let shape_ok = !legacy_only
                && guarded
                && !raw_mode
                && crate::x64::trusted_oop_receiver_getfield_enabled()
                && self.graph.nodes[base as usize].ty == IrType::Ref;
            let mode = if shape_ok {
                self.proven_receiver_mode(base, pc, true)
            } else {
                None
            };
            if mode.is_none() {
                self.proven_field_site_census("load", base, pc, true, shape_ok);
            }
            match mode {
                Some(mode) => {
                    self.emit_proven_receiver_getfield(
                        mode,
                        pc,
                        node_ty,
                        base,
                        field_index,
                        slot,
                        type_tag,
                        cell_off,
                        volatile_access,
                        colored_base,
                    );
                    if let Some(d) = self.cur_def {
                        self.attach_add_pad_remats(d, pads_before);
                    }
                    return true;
                }
                None => self.proven_field_census.load_full += 1,
            }
        }
        if let Some(operand) = null_tested_operand {
            self.gp_load_value(R11, operand);
            self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
            slow.push(self.emit_jcc_rel32(0x84)); // JZ -> helper
        }
        // `cell_off` is a compile-time claim about this class's compact layout.
        // Round 16 (lane fieldguard, FG16-1): possibly a deopting guard, or
        // none (`emit_field_layout_guard`); every `Op::Load` bci is already a
        // predicted deopt site (`op_cannot_deopt` does not list `Op::Load`).
        // A legacy-only site bakes no compact displacement, so it owes none.
        if !legacy_only && !self.block_cse_layout_epoch_checked() {
            slow.extend(self.emit_field_layout_guard(base, pc, true));
        }

        let receiver = self.bind_field_receiver(node_ty, base);
        // 2026-09-02: a receiver this block already proved (null-tested and,
        // where the guard applies, mapped) is not re-proved. Same SSA value,
        // same block, and no way for it to become null or unmapped in between:
        // a relocating collector rewrites the home word to the object's new
        // address, which is still mapped. `itemCheck` proved `n` once per
        // field it read (`ir_receiver_guard_cse_enabled`).
        let receiver_proven = self.receiver_already_guarded(base);
        // The null test is suppressed by the WEAKER fact as well: the seed, or
        // a test already emitted in this block. The containment guard below
        // still keys off `receiver_proven` alone, so a null-only proof never
        // licenses skipping the alignment and read-bounds compares.
        // JIT round 23 wave 9 (lane W9-L7, `CRATONVM_JIT_IR_GETFIELD_GUARD_NON_NULL`):
        // or by a `Guard(base != null)` earlier in this block -- the null-base
        // load predicate's guard ahead of its pre-header copies.
        if self.receiver_already_non_null(base)
            || self.block_guard_proves_non_null(base)
            || self
                .field_proof_block()
                .is_some_and(|b| self.dominating_instanceof_proves_non_null(base, b))
        {
            crate::metrics::note_ir_receiver_null_check_elided();
            self.note_receiver_non_null(base);
        } else {
            // 1. null → slow (the helper raises the NPE).
            self.emit_field_receiver_test(receiver);
            slow.push(self.emit_jcc_rel32(0x84)); // JZ
            crate::metrics::note_ir_receiver_null_check_emitted();
            // Its fall-through is a proof for the rest of the block.
            self.note_receiver_non_null(base);
        }
        let bounds_always_live = (self.ref_store_gates.is_none()
            && self.g1_barrier_addr != 0
            && self.read_bounds_addr != 0)
            || self.gen_bounds_always_live()
            || (self.zgc_fused_gate.is_some() && !crate::x64::zgc_inline_field_loads_gated());
        if guarded
            && !raw_mode
            && !trusted_oop_receiver
            && !receiver_proven
            && colored_base.is_none()
        {
            if trusted_ref_receiver {
                // Not a receiver proof: no `note_receiver_guarded`.
                if !bounds_always_live {
                    self.emit_read_bounds_live_check(&mut slow);
                }
            } else {
                self.note_receiver_guarded(base);
                self.emit_receiver_align_and_containment_at(&mut slow, receiver);
            }
        } else if ref_node && receiver_proven && guarded && !raw_mode && colored_base.is_none() {
            // JIT round 25 (fixed: `r25-receiver-cse-skips-the-read-table-check-FIXED-20261024.md`):
            // the block's receiver CSE proved the RECEIVER mapped, which a
            // later read may carry; it did not prove that reference slots
            // still hold plain pointers, which is a property of time -- the
            // collectors clear the table at a safepoint to send compiled
            // reference reads to the barriered helper. Ask that per read.
            if !bounds_always_live {
                self.emit_read_bounds_live_check(&mut slow);
            }
        }
        // Proxy class ids are outside the compact-layout registry. A
        // resolved primitive capture still keeps the receiver and slot checks.
        let legacy_capture = self.operand_in_place_on
            && legacy_only
            && guarded
            && trusted_oop_receiver
            && self
                .cur_def
                .and_then(|id| self.graph.field_decl_class.get(&id).copied())
                .is_some_and(crate::ir::ir_splice_body_is_lambda_forwarder);
        // 4. per-OBJECT compactness. A class with a registered compact layout
        //    can still have legacy 16-byte-cell instances — and in practice
        //    almost all of them are, because the TLAB fast path writes a legacy
        //    header unconditionally (see this function's doc comment). Reading
        //    one at the packed offset yields a mangled {tag, half-pointer}
        //    word, so the two layouts get two reads, exactly as the single-pass
        //    arm does.
        //    (JIT round 23 wave 3, lane L1: "almost all" is out of date. Since
        //    2026-09-03 `init_object_header` writes the compact shape, and the
        //    compiled inline `new` stamps `GC_FLAG_COMPACT`, so the compact arm
        //    below -- the fall-through -- is the common one; the
        //    `CRATONVM_DBG_COMPACT_LEGACY` census counts the legacy stamps that
        //    remain.)
        if !legacy_capture {
            self.emit_field_compact_test(receiver);
        }
        // JIT round 23 wave 9 (lane W9-L7, `CRATONVM_JIT_IR_GETFIELD_COLD_TAIL`):
        // the compact read falls straight through to the join, and the legacy
        // read and the helper go to a tail past the body
        // (`emit_deferred_getfield_tails`) -- one taken `JMP done` less per
        // read. Only where the tail can be emitted out of context: see
        // `getfield_cold_tail_admitted`.
        // JIT round 21 (lane tl3): a gated referent read keeps its legacy arm
        // inline, where the SATB pre-gate test follows the read.
        let outline = !referent_gate
            && colored_base.is_none()
            && ir_getfield_cold_tail_enabled()
            && self.getfield_cold_tail_admitted(base);
        let direct_result = if volatile_access {
            None
        } else if ref_node {
            // Referent and colored paths have additional post-load barriers.
            (colored_base.is_none() && !referent_gate && self.ref_residency_on)
                .then(|| self.reference_result_gpr(slot))
                .flatten()
                .filter(|&dst| dst != receiver)
        } else {
            self.integer_result_gpr(node_ty, slot)
        };
        let (legacy_patch, done_patches) = if legacy_only {
            // FG16-7: no compact row, so no compact displacement to read at.
            // A compact object (a class whose layout was registered after this
            // compile resolved its rows) takes the checked helper.
            if !legacy_capture {
                slow.push(self.emit_jcc_rel32(0x85)); // JNZ → helper
            }
            (None, Vec::new())
        } else {
            let legacy_patch = self.emit_jcc_rel32(0x84); // JZ → legacy inline read

            // 5. the read itself. A compact reference field is the bare 8-byte
            //    pointer at the cell base; a primitive is its tagless descriptor
            //    width.
            // zt t23/zb6: the color flip's two exits to the join (its head
            // replaces the `JMP` below).
            let mut flip_done: Vec<usize> = Vec::new();
            if ref_node {
                flip_done = self.emit_compact_reference_field_load(
                    receiver,
                    cell_off,
                    colored_base,
                    &mut slow,
                    direct_result.unwrap_or(RAX),
                );
                // JIT round 21 (lane g1vhsoft): a referent read with a row
                // (the `Reference.get` expansion's) keeps the legacy arm's
                // SATB keep-alive gate below, tested AFTER the read for the
                // same reason (`referent_gated_inline_admitted`): armed, the
                // checked helper re-reads the referent and logs it.
                if referent_gate {
                    self.emit_mov_reg_imm64(R11, self.satb_pre_gate as u64);
                    self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP byte [R11], 0
                    slow.push(self.emit_jcc_rel32(0x85)); // JNE -> the checked helper
                }
            } else {
                self.emit_compact_prim_field_load_to(
                    type_tag,
                    cell_off,
                    direct_result.unwrap_or(RAX),
                    receiver,
                );
            }
            if !flip_done.is_empty() {
                // Every compact exit is already placed (zt t23/zb6).
                (Some(legacy_patch), flip_done)
            } else if outline {
                // W9-L7: the compact read falls through to the join below.
                (Some(legacy_patch), Vec::new())
            } else {
                self.buf.emit_byte(0xE9); // JMP rel32 → done
                let done_patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                (Some(legacy_patch), vec![done_patch])
            }
        };

        if outline {
            // With no compact layout, the legacy read is the hot arm. Keep
            // its slot-count and tag checks, but send their failures to the
            // same outlined helper used by compact sites.
            if legacy_only {
                self.emit_legacy_slot_count_check(receiver, field_index_imm);
                slow.push(self.emit_jcc_rel32(0x86)); // JBE -> helper
                self.emit_getfield_legacy_read(
                    ref_node,
                    type_tag,
                    legacy_cell_disp(field_index as usize),
                    &mut slow,
                    direct_result.unwrap_or(RAX),
                    receiver,
                );
            }
            if legacy_patch.is_some() || legacy_only {
                let base_src = match self.resident_gpr(base) {
                    Some(r) => SelfCallArgSource::Reg(r),
                    None => SelfCallArgSource::Home(self.slot_of(base)),
                };
                let base_is_proven_oop = self.graph.nodes[base as usize].ty == IrType::Ref;
                let npe_site = self.record_npe_trap_site(pc);
                let arg2 = cratonvm_jit_api::getfield_index_arg(
                    field_index as u32,
                    ref_node,
                    base_is_proven_oop,
                    npe_site,
                ) | self.getfield_no_keepalive_bit(pc);
                // The tail's helper call writes RCX (the context argument) on
                // the path that reaches the join through it, as the inline
                // form did at this point: keep the RCX accounting the same.
                self.note_rcx_written();
                // The join: the compact read just emitted falls through to it.
                let done = self.buf.pos();
                // JIT round 25 item 2, wave 4: the lazy references the tail's
                // helper call spills and reloads, as the inline slow path.
                let lazy_spills = self.lazy_ref_pairs_here();
                self.deferred_getfield_tails.push(DeferredGetfieldTail {
                    receiver_gpr: receiver,
                    result_gpr: direct_result,
                    lazy_spills,
                    legacy_patch,
                    slow: std::mem::take(&mut slow),
                    done,
                    ref_node,
                    type_tag,
                    legacy_cell_off: legacy_cell_disp(field_index as usize),
                    node_ty,
                    base_src,
                    arg2,
                    site_bci: self.cur_bci,
                    node_pc: self.cur_node_pc,
                    implicit_nn_site: None,
                });
                if let Some(dst) = direct_result {
                    self.publish_integer_result(dst, slot);
                    if ref_node {
                        self.ref_def_home_stores += 1;
                    }
                } else {
                    self.store_rax(slot);
                }
                if !volatile_access && !referent_gate && !no_keepalive {
                    self.block_cse_record_field_load(
                        base,
                        field_index as u32,
                        node_ty,
                        cell_off,
                        type_tag,
                        true,
                    );
                }
                if let Some(d) = self.cur_def {
                    self.attach_add_pad_remats(d, pads_before);
                }
                return true;
            }
        }
        // --- legacy path: the uniform 16-byte `Value` cell. Transcribed from
        //     the single-pass arm's legacy branch; the payload sub-offsets and
        //     the sign-extension choice are that arm's, not a re-derivation. ---
        if let Some(p) = legacy_patch {
            self.patch_rel32_to_here(p);
        }
        if legacy_only {
            // FG16-7: an instance a native minted with fewer slots than the
            // class declares (`num_slots <= field_index`, unsigned) takes the
            // checked helper, which bounds-checks; the compact-row arm's
            // instances are never short (their class has a layout).
            self.emit_legacy_slot_count_check(receiver, field_index_imm);
            slow.push(self.emit_jcc_rel32(0x86)); // JBE → helper
        }
        let legacy_cell_off = legacy_cell_disp(field_index as usize);
        self.emit_getfield_legacy_read(
            ref_node,
            type_tag,
            legacy_cell_off,
            &mut slow,
            direct_result.unwrap_or(RAX),
            receiver,
        );
        // Armed, the payload word takes the compact arm's barrier (RAX: a
        // colored compile has no direct result register).
        let legacy_flip_done = match colored_base {
            Some(b) if ref_node => self.emit_colored_ref_word_barrier(b, &mut slow),
            _ => {
                if ref_node
                    && self.zgc_fused_gate.is_some()
                    && !crate::x64::zgc_inline_field_loads_gated()
                {
                    self.emit_zgc_plain_ref_word_guard(direct_result.unwrap_or(RAX), &mut slow);
                }
                Vec::new()
            }
        };
        // JIT round 21 (lane tl3, TL221-1): a referent read is kept only
        // while the SATB pre-gate byte is clear; armed, the checked helper
        // re-reads and logs it (the keep-alive). Tested AFTER the read, as
        // HotSpot's G1 `Reference.get` does: a peer a take-over freezes
        // between the two holds the referent in a register, a root of the
        // mark it starts (see `referent_gated_inline_admitted`).
        // `MOV R11, &gate ; CMP byte [R11], 0 ; JNE -> helper`.
        if ref_node && referent_gate {
            self.emit_mov_reg_imm64(R11, self.satb_pre_gate as u64);
            self.buf.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP byte [R11], 0
            slow.push(self.emit_jcc_rel32(0x85)); // JNE -> the checked helper
        }
        self.buf.emit_byte(0xE9); // JMP rel32 → done
        let done_legacy_patch = self.buf.pos();
        self.buf.emit(&[0; 4]);

        // --- slow path: the checked helper, byte-identical to the arm this
        //     replaces, including the sentinel bail. ---
        for p in slow {
            self.patch_rel32_to_here(p);
        }
        // For a REFERENCE field whose base node the IR already types `Ref`, use
        // the helper that skips the `is_object_address` membership walk.
        //
        // The walk is validation against a stale receiver, and the proof we
        // have here is the same one the PRIMITIVE trusted-oop arm above relies
        // on — an arm that goes further and does a raw inline load off this
        // very receiver. Handing it to a helper instead is a strictly weaker
        // use of the same trust.
        //
        // This changes only the SLOW path. The inline path is untouched, so
        // Generational — where containment passes and the inline ref load is
        // taken — sees no difference at all. It is ZGC and G1, which publish no
        // read bounds for reference loads, that take this path on 100% of
        // reference accesses, and there the walk was the largest single cost of
        // a field-dense run (`ZObjectStarts::contains` 11.1% +
        // `is_object_address` 9.5% on `dev` @800d17cc8).
        //
        // `contains` is already a tight bitmap probe; the cost is one
        // cache-missing random probe per access, so the fix is to stop asking,
        // not to ask faster.
        // Carried in `field_index`, NOT a new helper slot: `helpers_abi.rs`
        // pins the table's field count, byte size and golden offsets with const
        // assertions and an ABI version, all of which exist to keep the layout
        // the JIT bakes frozen. A one-bit argument flag needs none of that, and
        // the index is a small non-negative slot number with 62 spare bits.
        // The second flag, `GETFIELD_EXPECT_REFERENCE`, tells the helper what
        // we are going to DO with the answer rather than what we know about the
        // receiver: the code emitted after this call dereferences the result
        // for a reference field, while the helper otherwise returns the payload
        // of whichever `Value` variant it finds in the slot. A type-punned
        // primitive therefore became a wild pointer — `SQLChar.rawData` is
        // declared `[C`, read back as `Int(1)`, and the `arraylength` that
        // followed faulted at `addr=0x5`.
        self.emit_getfield_checked_helper_call(pc, node_ty, base, field_index, ref_node);

        if let Some(dst) = direct_result {
            self.emit_mov_reg_reg64(dst, RAX);
        }
        for p in done_patches.into_iter().chain(legacy_flip_done) {
            self.patch_rel32_to_here(p);
        }
        self.patch_rel32_to_here(done_legacy_patch);
        if let Some(dst) = direct_result {
            self.publish_integer_result(dst, slot);
            if ref_node {
                self.ref_def_home_stores += 1;
            }
        } else {
            self.store_rax(slot);
        }
        if !volatile_access && !referent_gate && !no_keepalive {
            self.block_cse_record_field_load(
                base,
                field_index as u32,
                node_ty,
                cell_off,
                type_tag,
                true,
            );
        }
        if let Some(d) = self.cur_def {
            self.attach_add_pad_remats(d, pads_before);
        }
        true
    }

    /// The checked `jit_getfield` call of an inline field read's slow path,
    /// with its width-aware sentinel bail; the answer is left in RAX. Shared
    /// by [`Self::emit_inline_compact_getfield`] (whose comment above the call
    /// says why the flags in `arg2` are what they are) and
    /// [`Self::emit_proven_receiver_getfield`], so the two slow paths cannot
    /// drift.
    /// t1 wave 10 (lane REFS3, `CRATONVM_JIT_IR_REFERS_TO_NO_KEEPALIVE`):
    /// `GETFIELD_NO_KEEPALIVE` when the load being lowered (`cur_def`, an
    /// `Op::Load` at `pc`) is a compare-only referent read the builder listed
    /// in `Graph::no_keepalive_referent_loads` (the `refersTo` expansions),
    /// else `0`. The helper then skips its slot-0 SATB keep-alive, as
    /// HotSpot's `AS_NO_KEEPALIVE` `refersTo0` load does.
    /// The one value every value use of the load being lowered (`cur_def`)
    /// compares it with (`==` / `!=`), or `None` for any other use or two
    /// different operands. See `referent_read_owes_the_checked_helper`.
    pub(in crate::ir_lower) fn compare_only_read_operand(&self) -> Option<NodeId> {
        let load = self.cur_def?;
        let mut operand: Option<NodeId> = None;
        for user in self.graph.nodes.iter() {
            if user.op == Op::Dead {
                continue;
            }
            for (slot, &input) in user.inputs.iter().enumerate() {
                if input != load || crate::ir::is_memory_token_slot(user, slot) {
                    continue;
                }
                if !(matches!(user.op, Op::Cmp(CmpOp::Eq | CmpOp::Ne)) && user.inputs.len() == 2 && slot < 2) {
                    return None;
                }
                let other = user.inputs[1 - slot];
                if other == load || operand.is_some_and(|o| o != other) {
                    return None;
                }
                operand = Some(other);
            }
        }
        operand
    }

    /// Can node `id` never be null: an allocation or constant materializer
    /// (`definitely_non_null`), a constant root (`IndyRootRef`, a
    /// never-released global slot of an object), or an `Object.getClass()`
    /// call's answer?
    pub(in crate::ir_lower) fn node_never_null(&self, id: NodeId) -> bool {
        self.graph.nodes.get(id as usize).is_some_and(|n| {
            crate::ir_check_elim::definitely_non_null(&n.op)
                || matches!(n.op, Op::IndyRootRef { .. })
                || matches!(n.op, Op::Call { info_ptr } if info_ptr != 0 && {
                    // SAFETY: a live `JitInvokeInfo` of this compile.
                    let i: &crate::JitInvokeInfo = unsafe { &*(info_ptr as *const crate::JitInvokeInfo) };
                    i.invoke_kind == 0 && crate::is_object_get_class_site(i.method_name, i.descriptor)
                })
        })
    }

    pub(in crate::ir_lower) fn getfield_no_keepalive_bit(&self, pc: usize) -> u64 {
        let listed = self.cur_def.is_some_and(|d| {
            self.graph.no_keepalive_referent_loads.contains(&d)
                && self
                    .graph
                    .nodes
                    .get(d as usize)
                    .is_some_and(|n| matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(pc))
        });
        if listed {
            cratonvm_jit_api::GETFIELD_NO_KEEPALIVE
        } else {
            0
        }
    }

    pub(in crate::ir_lower) fn emit_getfield_checked_helper_call(
        &mut self,
        pc: usize,
        node_ty: IrType,
        base: NodeId,
        field_index: i64,
        ref_node: bool,
    ) {
        let base_is_proven_oop = self.graph.nodes[base as usize].ty == IrType::Ref;
        // The NPE trap-site key, so a null receiver reaching the helper can
        // carry JEP 358's message out: the helper has the receiver (null) and
        // the slot index, and neither names the field or the bci.
        let npe_site = self.record_npe_trap_site(pc);
        let arg2 = cratonvm_jit_api::getfield_index_arg(
            field_index as u32,
            ref_node,
            base_is_proven_oop,
            npe_site,
        ) | self.getfield_no_keepalive_bit(pc);
        // JIT round 25 item 2, wave 4: the lazy references live here go to
        // their homes for the helper's collection, and come back after it.
        let lazy = self.lazy_ref_hook_enter();
        self.load_context(CALL_ARG_REGS[0]);
        self.gp_load_value(CALL_ARG_REGS[1], base);
        self.emit_mov_reg_imm64(CALL_ARG_REGS[2], arg2);
        self.emit_mov_reg_imm64(RAX, self.getfield as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
                                      // WIDTH-AWARE, like the helper-only arm in `Op::Load` and the
                                      // single-pass slow path (`emit_post_invoke_exception_check`). Round 11
                                      // (irlower): this was a bare `CMP RAX, 1; JO bail` for every width,
                                      // which stopped being "byte-identical to the arm this replaces" the day
                                      // `J`/`D` were admitted above -- a `long` field holding `Long.MIN_VALUE`
                                      // or a `double` holding `-0.0` (bits == the sentinel) that reached this
                                      // slow path (a layout-epoch bump, a failed containment test) left the
                                      // method as if an exception were pending, with none pending.
        self.emit_helper_sentinel_check(node_ty);
        self.lazy_ref_hook_exit(lazy);
    }

    /// Select an existing register without opting into home elision. Carries,
    /// split ranges and MIR keep their own publication contracts.
    pub(in crate::ir_lower) fn integer_result_gpr(&self, ty: IrType, slot: i32) -> Option<u8> {
        if !matches!(ty, IrType::Int | IrType::Long)
            || !(self.reg_alloc_on || self.operand_in_place_on)
            || !self.publish_at_def_on
            || self.mir_mode != MirMode::Off
            || self.deferred_rcx.is_some()
            || self.tail_carry_at_store(slot).is_some()
            || self.carry_at_store(slot).is_some()
        {
            return None;
        }
        let id = self.cur_def?;
        if self.ls_managed.get(id as usize).copied().unwrap_or(false) {
            return None;
        }
        self.assigned_gpr(id)
            .filter(|&reg| reg != RAX && reg != RCX)
    }

    /// Direct reference results keep their authoritative frame home for GC and
    /// deoptimization; lazy, carried and split-managed values keep the legacy
    /// publication path until their transition contracts are unified.
    pub(in crate::ir_lower) fn reference_result_gpr(&self, slot: i32) -> Option<u8> {
        let id = self.cur_def?;
        if self.graph.nodes.get(id as usize)?.ty != IrType::Ref
            || self.home_offset_of(id) != Some(slot)
            || self.home_dropped.get(id as usize).copied().unwrap_or(false)
            || self
                .lazy_ref_home
                .get(id as usize)
                .copied()
                .unwrap_or(false)
        {
            return None;
        }
        // The integer selector applies the shared carry, split and MIR gates.
        self.integer_result_gpr(IrType::Long, slot)
    }

    /// Compare resident long operands without staging either through RAX/RCX.
    /// The caller materializes the three-way result in RAX/RDX, so those
    /// registers cannot hold either input on this route.
    pub(in crate::ir_lower) fn try_allocated_lcmp(&mut self, a: NodeId, b: NodeId) -> bool {
        if !(self.reg_alloc_on || self.operand_in_place_on)
            || self.mir_mode != MirMode::Off
            || self.carry_names(a)
            || self.carry_names(b)
            || self
                .tail_carry
                .is_some_and(|(producer, _, _)| producer == a || producer == b)
        {
            return false;
        }
        let (Some(lhs), Some(rhs)) = (self.resident_gpr(a), self.resident_gpr(b)) else {
            return false;
        };
        if [lhs, rhs].iter().any(|&reg| reg == RAX || reg == RDX) {
            return false;
        }
        self.emit_cmp_reg_reg(lhs, rhs, true);
        true
    }

    /// True when `id` has at least one `Op::I2L` consumer in the graph.
    pub(in crate::ir_lower) fn has_i2l_consumer(&self, id: NodeId) -> bool {
        self.graph
            .nodes
            .iter()
            .any(|n| n.op == Op::I2L && n.inputs.first().copied() == Some(id))
    }

    /// True when our x64 lowering leaves `input` (`IrType::Int`) already
    /// 64-bit sign-extended in both GPR and home word, so downstream `Op::I2L`
    /// needs no `MOVSXD`. Never true under `CRATONVM_DBG_IR_POISON_INT_HIGH`.
    pub(in crate::ir_lower) fn int_already_sign_extended(&self, input: NodeId) -> bool {
        if self.poison_int_high_on {
            return false;
        }
        let Some(node) = self.graph.nodes.get(input as usize) else {
            return false;
        };
        if node.ty != IrType::Int {
            return false;
        }
        match node.op {
            Op::Load(kind) | Op::ArrayLoad(kind) => matches!(
                kind,
                MemKind::Int | MemKind::Short | MemKind::Byte | MemKind::Char
            ),
            Op::Unbox { op, .. } => !op.is_wide() && self.has_i2l_consumer(input),
            _ => false,
        }
    }

    /// Finish an `Op::Unbox` node: post-add for XADD variants, sign-extend when
    /// an `Op::I2L` consumes the `Int` result, and publish into GPR / home.
    pub(in crate::ir_lower) fn finish_unbox_result(
        &mut self,
        id: NodeId,
        uop: crate::ir::UnboxOp,
        slot: i32,
    ) {
        let wide = uop.is_wide();
        if !uop.is_load() {
            if uop.returns_post_add() {
                if wide {
                    self.buf.emit(&[0x48, 0x01, 0xD1]); // ADD RCX, RDX
                } else {
                    self.buf.emit(&[0x01, 0xD1]); // ADD ECX, EDX
                }
            }
            if wide {
                self.buf.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
            } else {
                self.buf.emit(&[0x89, 0xC8]); // MOV EAX, ECX
            }
        }
        let node_ty = self
            .graph
            .nodes
            .get(id as usize)
            .map_or(IrType::Void, |n| n.ty);
        if !wide && node_ty == IrType::Int && !self.poison_int_high_on && self.has_i2l_consumer(id)
        {
            self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
        }
        if let Some(dst) = self.integer_result_gpr(node_ty, slot) {
            if dst != RAX {
                self.emit_mov_reg_reg64(dst, RAX);
            }
            self.publish_integer_result(dst, slot);
        } else {
            self.store_rax(slot);
        }
    }

    /// Widen an already resident int directly into its assigned result register.
    /// Values carried by the legacy RAX protocol keep the existing path.
    pub(in crate::ir_lower) fn try_allocated_i2l(
        &mut self,
        id: NodeId,
        input: NodeId,
        slot: i32,
    ) -> bool {
        if self.carry_names(input)
            || self
                .tail_carry
                .is_some_and(|(producer, _, _)| producer == input)
        {
            return false;
        }
        let Some(dst) = self.integer_result_gpr(IrType::Long, slot) else {
            return false;
        };
        let Some(src) = self.resident_gpr(input) else {
            return false;
        };
        use crate::x64::isel::{select, Op as MachineOp, Operand, Req, Ty};
        if self.int_already_sign_extended(input) {
            if dst != src {
                let Ok(code) = select(&Req::new(
                    MachineOp::Mov,
                    Ty::I64,
                    Operand::Gpr(dst),
                    Operand::Gpr(src),
                )) else {
                    return false;
                };
                self.buf.emit(&code.encoded.bytes);
            }
        } else {
            let Ok(code) = select(&Req::new(
                MachineOp::Movsxd,
                Ty::I32,
                Operand::Gpr(dst),
                Operand::Gpr(src),
            )) else {
                return false;
            };
            // The selected operation reads src before publishing dst, including
            // when both names share a register. No safepoint lies between them.
            self.buf.emit(&code.encoded.bytes);
        }
        self.publish_integer_result(dst, slot);
        debug_assert_eq!(self.cur_def, Some(id));
        true
    }

    /// Every admitted integer producer publishes its selected value here.
    /// Keep the existing deopt home and poison convention; only its producer moves.
    pub(in crate::ir_lower) fn publish_integer_result(&mut self, dst: u8, slot: i32) {
        let Some(id) = self.cur_def else { return };
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
            if self.home_offset_of(id) == Some(slot) {
                self.def_home_stores += 1;
                self.note_home_keep(id);
                if let Some(cell) = self.own_home_written.get_mut(id as usize) {
                    *cell = slot;
                }
            }
        }
        self.reg_alloc_direct_defs += 1;
        self.rax_home_mirror = None;
        self.rax_reg_mirror = None;
    }

    /// Keep an already allocated array receiver in its register through the
    /// null test and the length load. The deopt edge keeps its original frame.
    pub(in crate::ir_lower) fn lower_allocated_array_length(
        &mut self,
        id: NodeId,
        array: NodeId,
        slot: i32,
        bci: usize,
    ) -> bool {
        use crate::x64::isel::{select, Mem, Op as MachineOp, Operand, Req, Ty};
        if !self.reg_alloc_on || self.carry_names(array) {
            return false;
        }
        let Some(base) = self.resident_gpr(array) else {
            return false;
        };
        let dst = self.integer_result_gpr(IrType::Int, slot);
        let receiver = Operand::Gpr(base);
        let Ok(test) = select(&Req::new(MachineOp::Test, Ty::I64, receiver, receiver)) else {
            return false;
        };
        let Ok(load) = select(&Req::new(
            MachineOp::Mov,
            Ty::I32,
            Operand::Gpr(dst.unwrap_or(RAX)),
            Operand::Mem(Mem::base_disp(base, ARRAY_LENGTH_OFFSET as i64)),
        )) else {
            return false;
        };
        let elide_null = self.check_elision.null_elided(id);
        crate::ir_check_elim::note_check(0, elide_null);
        if !elide_null {
            self.buf.emit(&test.encoded.bytes);
            self.emit_deopt_if_zero(bci, DeoptReason::NullCheck);
        }
        self.buf.emit(&load.encoded.bytes);
        if let Some(dst) = dst {
            self.publish_integer_result(dst, slot);
        } else {
            self.store_rax(slot);
        }
        true
    }

    /// The compact read of a primitive field into an arbitrary GP register.
    /// The receiver and destination may name the same register.
    pub(in crate::ir_lower) fn emit_compact_prim_field_load_to(
        &mut self,
        type_tag: u8,
        cell_off: i32,
        dst: u8,
        receiver: u8,
    ) {
        use crate::x64::isel::{Mem, Op as MachineOp, Operand, Req, Ty};
        let (op, ty) = match type_tag {
            b'Z' => (MachineOp::Movzx, Ty::I8),
            b'B' => (MachineOp::Movsx, Ty::I8),
            b'C' => (MachineOp::Movzx, Ty::I16),
            b'S' => (MachineOp::Movsx, Ty::I16),
            b'J' | b'D' => (MachineOp::Mov, Ty::I64),
            b'F' => (MachineOp::Mov, Ty::I32),
            _ => (MachineOp::Movsxd, Ty::I32),
        };
        self.emit_field_instruction(Req::new(
            op,
            ty,
            Operand::Gpr(dst),
            Operand::Mem(Mem::base_disp32(receiver, i64::from(cell_off))),
        ));
    }

    pub(in crate::ir_lower) fn emit_field_instruction(&mut self, req: crate::x64::isel::Req) {
        match crate::x64::isel::select(&req) {
            Ok(inst) => self.buf.emit(&inst.encoded.bytes),
            Err(_) => self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "field instruction has no encoding",
            ))),
        }
    }

    pub(in crate::ir_lower) fn emit_field_receiver_test(&mut self, receiver: u8) {
        use crate::x64::isel::{Op as MachineOp, Operand, Req, Ty};
        let reg = Operand::Gpr(receiver);
        self.emit_field_instruction(Req::new(MachineOp::Test, Ty::I64, reg, reg));
    }

    pub(in crate::ir_lower) fn emit_field_compact_test(&mut self, receiver: u8) {
        use crate::x64::isel::{Mem, Op as MachineOp, Operand, Req, Ty};
        self.emit_field_instruction(Req::new(
            MachineOp::Test,
            Ty::I8,
            Operand::Mem(Mem::base_disp32(
                receiver,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as i64,
            )),
            Operand::Imm(i64::from(cratonvm_types::GC_FLAG_COMPACT)),
        ));
    }

    pub(in crate::ir_lower) fn emit_legacy_slot_count_check(
        &mut self,
        receiver: u8,
        field_index: i32,
    ) {
        use crate::x64::isel::{Mem, Op as MachineOp, Operand, Req, Ty};
        self.emit_field_instruction(Req::new(
            MachineOp::Cmp,
            Ty::I32,
            Operand::Mem(Mem::base_disp32(
                receiver,
                i64::from(legacy_num_slots_disp()),
            )),
            Operand::Imm(i64::from(field_index)),
        ));
    }

    /// Read a resident receiver through its assigned register. Guard scratch
    /// and the context argument must leave that register intact up to the read
    /// or checked helper's receiver argument.
    pub(in crate::ir_lower) fn bind_field_receiver(&mut self, _ty: IrType, base: NodeId) -> u8 {
        if (self.reg_alloc_on || self.operand_in_place_on)
            && self.mir_mode == MirMode::Off
            && !self.carry_names(base)
        {
            if let Some(reg) = self.resident_gpr(base) {
                if !matches!(reg, RAX | RCX | RDX | R10 | R11) && reg != CALL_ARG_REGS[0] {
                    self.gp_reg_reads += 1;
                    return reg;
                }
            }
        }
        self.gp_load_value(RAX, base);
        RAX
    }

    /// The compact store of a PRIMITIVE field: RDX (already narrowed to the
    /// field's width) at absolute displacement `cell_off` from the receiver
    /// in RAX, the field's own width. Shared by the full inline `putfield`
    /// and the proven-receiver store (item 4).
    pub(in crate::ir_lower) fn emit_compact_prim_field_store(
        &mut self,
        type_tag: u8,
        cell_off: i32,
    ) {
        match type_tag {
            b'Z' | b'B' => self.buf.emit(&[0x88, 0x90]), // MOV byte [RAX+disp32], DL
            b'C' | b'S' => self.buf.emit(&[0x66, 0x89, 0x90]), // MOV word [RAX+disp32], DX
            b'J' | b'D' => self.buf.emit(&[0x48, 0x89, 0x90]), // MOV qword [RAX+disp32], RDX
            _ => self.buf.emit(&[0x89, 0x90]),           // MOV dword [RAX+disp32], EDX
        }
        self.buf.emit(&cell_off.to_le_bytes());
    }

    pub(in crate::ir_lower) fn emit_compact_prim_field_store_at(
        &mut self,
        type_tag: u8,
        cell_off: i32,
        receiver: u8,
        source: ArrayStoreSource,
    ) {
        if receiver == RAX && matches!(source, ArrayStoreSource::Reg(RDX)) {
            self.emit_compact_prim_field_store(type_tag, cell_off);
            return;
        }
        use crate::x64::isel::{Mem, Op as MachineOp, Operand, Req, Ty};
        let addr = Operand::Mem(Mem::base_disp32(receiver, i64::from(cell_off)));
        match source {
            ArrayStoreSource::IntImm(imm) => {
                debug_assert!(matches!(type_tag, b'I' | b'F'));
                self.emit_field_instruction(Req::new(
                    MachineOp::Mov,
                    Ty::I32,
                    addr,
                    Operand::Imm(i64::from(imm)),
                ));
            }
            ArrayStoreSource::Reg(src) => {
                let ty = match type_tag {
                    b'Z' | b'B' => Ty::I8,
                    b'C' | b'S' => Ty::I16,
                    b'J' | b'D' => Ty::I64,
                    _ => Ty::I32,
                };
                self.emit_field_instruction(Req::new(MachineOp::Mov, ty, addr, Operand::Gpr(src)));
            }
        }
    }

    /// Emit a proven compact primitive `putfield` directly from resident receiver
    /// and source registers without staging through `RAX`/`RDX` or emitting a
    /// dead helper call.
    pub(in crate::ir_lower) fn try_emit_allocated_prim_putfield(
        &mut self,
        id: NodeId,
        bci: usize,
        node_pc: Option<usize>,
        kind: MemKind,
        base: NodeId,
        value: NodeId,
    ) -> bool {
        if !self.reg_alloc_on || self.mir_mode != MirMode::Off {
            return false;
        }
        let armed_trusted = crate::x64::zgc_armed_inline_primitive_fields_enabled()
            && crate::x64::trusted_oop_receiver_getfield_enabled()
            && self
                .graph
                .nodes
                .get(base as usize)
                .is_some_and(|n| n.ty == IrType::Ref);
        if !self.inline_prim_putfield_on
            || !crate::x64::inline_putfield_enabled()
            || (crate::x64::narrow_oops_block_inline_fields() && !armed_trusted)
            || !cratonvm_types::compact_ref_fields_enabled()
            || !self.proven_fields_on
        {
            return false;
        }
        let Some(pc) = node_pc else { return false };
        let row = if self.graph.synthetic_field_access_pcs.contains(&pc) {
            self.synthetic_store_row
                .filter(|&(_, is_ref, _)| !is_ref && ir_synthetic_store_field_row_enabled())
        } else {
            self.compact_fields.get(&(pc, false)).copied()
        };
        let Some((c_off, c_is_ref, type_tag)) = row else {
            return false;
        };
        let width_ok = match type_tag {
            b'I' | b'Z' | b'B' | b'C' | b'S' => kind == MemKind::Int,
            b'J' => kind == MemKind::Long,
            b'F' => kind == MemKind::Float,
            b'D' => kind == MemKind::Double,
            _ => false,
        };
        if c_is_ref || !width_ok {
            return false;
        }
        let trusted_oop_receiver = crate::x64::trusted_oop_receiver_getfield_enabled()
            && self
                .graph
                .nodes
                .get(base as usize)
                .is_some_and(|n| n.ty == IrType::Ref);
        if !trusted_oop_receiver {
            return false;
        }
        let Ok(cell_off) = i32::try_from(c_off) else {
            return false;
        };
        let store_may_deopt = !store_receiver_cannot_be_null(self.graph, base);
        let Some(mode) = self.proven_receiver_mode(base, pc, store_may_deopt) else {
            return false;
        };
        if self.field_layout_guard_rejoins(base, pc, store_may_deopt) {
            return false;
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
        self.emit_store_receiver_null_check_at(base, bci, id, receiver);
        let epoch = self.emit_field_layout_guard(base, pc, store_may_deopt);
        debug_assert!(epoch.is_none());
        let source =
            if matches!(type_tag, b'I' | b'J' | b'B' | b'S' | b'C') && !self.carry_names(value) {
                if let Some(reg) = self
                    .resident_gpr(value)
                    .filter(|r| IR_LOWER_LS_GPRS.contains(r) || IR_LS_CALLER_SAVED_GPRS.contains(r))
                {
                    self.gp_reg_reads += 1;
                    Some(ArrayStoreSource::Reg(reg))
                } else if type_tag == b'I' && self.const_imm_on {
                    self.alu_imm32(value).map(ArrayStoreSource::IntImm)
                } else {
                    None
                }
            } else if type_tag == b'Z'
                && !self.carry_names(value)
                && self.int_value_is_boolean_bits(value)
            {
                if let Some(reg) = self
                    .resident_gpr(value)
                    .filter(|r| IR_LOWER_LS_GPRS.contains(r) || IR_LS_CALLER_SAVED_GPRS.contains(r))
                {
                    self.gp_reg_reads += 1;
                    Some(ArrayStoreSource::Reg(reg))
                } else {
                    None
                }
            } else {
                None
            }
            .unwrap_or_else(|| {
                if let Some(src) = self.resident_gpr(value).filter(|r| {
                    !self.carry_names(value)
                        && (IR_LOWER_LS_GPRS.contains(r) || IR_LS_CALLER_SAVED_GPRS.contains(r))
                        && *r != receiver
                }) {
                    self.gp_reg_reads += 1;
                    self.emit_mov_reg_reg64(RDX, src);
                } else {
                    self.gp_load_value(RDX, value);
                }
                if type_tag == b'Z' && !self.int_value_is_boolean_bits(value) {
                    self.buf.emit(&[0x83, 0xE2, 0x01]); // AND EDX, 1
                }
                ArrayStoreSource::Reg(RDX)
            });
        if mode == ProvenReceiverMode::Establish {
            self.emit_compact_receiver_trap_at(base, pc, true, receiver);
            self.proven_field_census.store_establish += 1;
        } else {
            self.proven_field_census.store_proven += 1;
        }
        self.emit_compact_prim_field_store_at(type_tag, cell_off, receiver, source);
        true
    }

    /// A compact reference read with the collector's existing decode or
    /// color-flip barrier. Returned branches join after the checked helper.
    pub(in crate::ir_lower) fn emit_compact_reference_field_load(
        &mut self,
        receiver: u8,
        cell_off: i32,
        colored_base: Option<u64>,
        slow: &mut Vec<usize>,
        dst: u8,
    ) -> Vec<usize> {
        // Colored barriers consume and produce RAX. Plain references may be
        // loaded directly into the result's assigned register.
        debug_assert!(colored_base.is_none() || dst == RAX);
        if receiver == RAX && dst == RAX {
            self.buf.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
            self.buf.emit(&cell_off.to_le_bytes());
        } else {
            use crate::x64::isel::{Mem, Op as MachineOp, Operand, Req, Ty};
            self.emit_field_instruction(Req::new(
                MachineOp::Mov,
                Ty::I64,
                Operand::Gpr(dst),
                Operand::Mem(Mem::base_disp32(receiver, i64::from(cell_off))),
            ));
        }
        match colored_base {
            Some(base) => self.emit_colored_ref_word_barrier(base, slow),
            None => {
                if self.zgc_fused_gate.is_some() && !crate::x64::zgc_inline_field_loads_gated() {
                    self.emit_zgc_plain_ref_word_guard(dst, slow);
                }
                Vec::new()
            }
        }
    }

    /// The colored load barrier on the reference word just loaded into RAX:
    /// the color flip's head (its plain and good exits returned, its bad edge
    /// pushed onto `slow`, where the checked helper rereads and heals) or the
    /// plain decode. A compact slot and a legacy cell's Object payload are the
    /// same kind of word to the collector (`ZgcRealHeap::get_field` barriers
    /// both), so both reads end here.
    pub(in crate::ir_lower) fn emit_colored_ref_word_barrier(
        &mut self,
        base: u64,
        slow: &mut Vec<usize>,
    ) -> Vec<usize> {
        {
            if let Some(cell) = crate::x64::zgc_colored_field_flip_bad_mask_cell() {
                let (plain, good) =
                    crate::x64::emit_zgc_colored_flip_head(&mut self.buf, cell, base);
                // The checked helper rereads and heals a bad-colored slot.
                slow.push(self.emit_jmp_rel32());
                if crate::x64::dbg_jitc_enabled() {
                    eprintln!("[cratonvm-jitc] ir zgc-color-flip-getfield");
                }
                return vec![plain, good];
            }
            crate::x64::emit_zgc_colored_slot_decode(&mut self.buf, base);
            if crate::x64::dbg_jitc_enabled() {
                eprintln!("[cratonvm-jitc] ir zgc-colored-inline-getfield");
            }
        }
        Vec::new()
    }

    /// JIT round 25 (item 4, `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS`): the
    /// inline read of a compact field of a `Ref`-typed receiver whose
    /// compactness is proven ([`ProvenReceiverMode::Proven`]) or proven here
    /// ([`ProvenReceiverMode::Establish`]); see [`Self::proven_receiver_mode`]
    /// for when and why. The caller has passed every decline of
    /// [`Self::emit_inline_compact_getfield`] (a compact row, descriptor and
    /// width agreement and guarded receiver semantics).
    ///
    /// ```text
    ///   [epoch guard: elided / deopting / rejoining]  ; emit_field_layout_guard
    ///   bind receiver (assigned register or RAX)
    ///   Establish: [test receiver, receiver ; jz slow]
    ///              test byte [receiver+flags], COMPACT ; jz <deopt pc>
    ///   ref field: plain-slot check + MOV, or MOV + colored load barrier
    ///   primitive: load dst, [receiver + cell_off]    ; descriptor width
    ///   (jmp done ; slow: jit_getfield ; done:)        ; only if a slow edge exists
    /// ```
    ///
    /// No legacy arm: a legacy object never gets past the guard. A `Proven`
    /// primitive read whose epoch guard is elided or deopting has no slow
    /// path at all.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::ir_lower) fn emit_proven_receiver_getfield(
        &mut self,
        mode: ProvenReceiverMode,
        pc: usize,
        node_ty: IrType,
        base: NodeId,
        field_index: i64,
        slot: i32,
        type_tag: u8,
        cell_off: i32,
        volatile_access: bool,
        colored_base: Option<u64>,
    ) {
        let ref_node = node_ty == IrType::Ref;
        let mut slow: Vec<usize> = Vec::new();
        // The baked displacement's epoch: `Establish` was admitted only where
        // this emits nothing that rejoins; a `Proven` site may still get the
        // rejoining compare (the compactness proof does not need the epoch).
        let epoch = if self.block_cse_layout_epoch_checked() {
            None
        } else {
            self.emit_field_layout_guard(base, pc, true)
        };
        let epoch_rejoins = epoch.is_some();
        slow.extend(epoch);
        let receiver = self.bind_field_receiver(node_ty, base);
        let mut implicit_nn_site: Option<usize> = None;
        match mode {
            ProvenReceiverMode::Proven => {
                // The proof's guard sat past a null test whose failure never
                // continues (the helper's NPE), so `base` is non-null here.
                crate::metrics::note_ir_receiver_null_check_elided();
                self.note_receiver_non_null(base);
                self.proven_field_census.load_proven += 1;
            }
            ProvenReceiverMode::Establish => {
                let exact_proves_nn = self
                    .cur_def
                    .is_some_and(|def| self.check_elision.null_elided(def))
                    || self.field_proof_block().is_some_and(|b| {
                        self.dominating_exact_class(base, b).is_some()
                            || self.dominating_instanceof_proves_non_null(base, b)
                    });
                let try_implicit_nn = !(self.receiver_already_non_null(base)
                    || self.block_guard_proves_non_null(base)
                    || exact_proves_nn)
                    && crate::implicit_null::active()
                    && self.implicit_throw_plans.is_empty()
                    && self
                        .graph
                        .proven_compact_withdrawn_pcs
                        .as_ref()
                        .is_some_and(|w| !w.contains(&pc));
                if self.receiver_already_non_null(base)
                    || self.block_guard_proves_non_null(base)
                    || exact_proves_nn
                {
                    crate::metrics::note_ir_receiver_null_check_elided();
                    if exact_proves_nn {
                        self.note_receiver_non_null(base);
                    }
                } else if try_implicit_nn {
                    crate::metrics::note_ir_receiver_null_check_elided();
                    self.note_receiver_non_null(base);
                } else {
                    self.emit_field_receiver_test(receiver);
                    slow.push(self.emit_jcc_rel32(0x84)); // JZ -> helper (the NPE)
                    crate::metrics::note_ir_receiver_null_check_emitted();
                    self.note_receiver_non_null(base);
                }
                if try_implicit_nn {
                    implicit_nn_site = Some(self.buf.pos());
                }
                // Recorded only with no rejoining edge ahead of it (the null
                // edge does not rejoin). `proven_receiver_mode` predicted none.
                self.emit_compact_receiver_trap_at(base, pc, !epoch_rejoins, receiver);
                self.proven_field_census.load_establish += 1;
                // W6-E (`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_COESTABLISH`):
                // here, past the guard and before any slow edge that rejoins.
                if !epoch_rejoins && self.proven_coestablish_on {
                    self.coestablish_receiver_proof(base, pc, receiver);
                }
            }
        }
        let direct_result = if volatile_access {
            None
        } else if ref_node {
            // The colored barrier owns RAX. Keep reference homes and avoid
            // clobbering the receiver before all slow-path checks have joined.
            (colored_base.is_none() && self.ref_residency_on)
                .then(|| self.reference_result_gpr(slot))
                .flatten()
                .filter(|&dst| dst != receiver)
        } else {
            self.integer_result_gpr(node_ty, slot)
        };
        let mut done_patches = Vec::new();
        let bounds_always_live = (self.ref_store_gates.is_none()
            && self.g1_barrier_addr != 0
            && self.read_bounds_addr != 0)
            || self.gen_bounds_always_live()
            || (self.zgc_fused_gate.is_some() && !crate::x64::zgc_inline_field_loads_gated());
        if ref_node {
            // Receiver proofs do not prove the current slot color. Keep the
            // plain-slot check or colored barrier on every reference read, except
            // G1/Gen where `JIT_READ_BOUNDS` stays live until VM teardown (and
            // ZGC plain mode uses `emit_zgc_plain_ref_word_guard`).
            if colored_base.is_none() && !bounds_always_live {
                self.emit_read_bounds_live_check(&mut slow);
            }
            done_patches = self.emit_compact_reference_field_load(
                receiver,
                cell_off,
                colored_base,
                &mut slow,
                direct_result.unwrap_or(RAX),
            );
        } else {
            self.emit_compact_prim_field_load_to(
                type_tag,
                cell_off,
                direct_result.unwrap_or(RAX),
                receiver,
            );
        }
        // The checked helper may go out of line under the same admission as
        // an ordinary field read. On the common path the compact read then
        // falls through to its result join without jumping over that helper.
        // Colored reads have their own multi-exit barrier and stay inline.
        // An implicit-null site defers too: its fault's recovery pc becomes
        // the tail's helper entry (`emit_deferred_getfield_tails_with_fault_entries`).
        // Captured before deferral empties `slow`: only a guard edge (or a
        // multi-exit load) can come back to the join with the helper's state.
        let slow_edges = !slow.is_empty();
        let load_rejoins = !done_patches.is_empty();
        if (slow_edges || implicit_nn_site.is_some())
            && done_patches.is_empty()
            && colored_base.is_none()
            && ir_getfield_cold_tail_enabled()
            && self.getfield_cold_tail_admitted(base)
        {
            let base_src = match self.resident_gpr(base) {
                Some(r) => SelfCallArgSource::Reg(r),
                None => SelfCallArgSource::Home(self.slot_of(base)),
            };
            let npe_site = self.record_npe_trap_site(pc);
            let arg2 = cratonvm_jit_api::getfield_index_arg(
                field_index as u32,
                ref_node,
                self.graph.nodes[base as usize].ty == IrType::Ref,
                npe_site,
            ) | self.getfield_no_keepalive_bit(pc);
            self.note_rcx_written();
            let done = self.buf.pos();
            let lazy_spills = self.lazy_ref_pairs_here();
            self.deferred_getfield_tails.push(DeferredGetfieldTail {
                receiver_gpr: receiver,
                result_gpr: direct_result,
                lazy_spills,
                legacy_patch: None,
                slow: std::mem::take(&mut slow),
                done,
                ref_node,
                type_tag,
                legacy_cell_off: legacy_cell_disp(field_index as usize),
                node_ty,
                base_src,
                arg2,
                site_bci: self.cur_bci,
                node_pc: self.cur_node_pc,
                implicit_nn_site: implicit_nn_site.take(),
            });
        }
        if !slow.is_empty() || implicit_nn_site.is_some() {
            if done_patches.is_empty() {
                done_patches.push(self.emit_jmp_rel32());
            }
            for &p in &slow {
                self.patch_rel32_to_here(p);
            }
            if let Some(site) = implicit_nn_site {
                self.page_poll_sites.push((site, self.buf.pos()));
            }
            self.emit_getfield_checked_helper_call(pc, node_ty, base, field_index, ref_node);
            if let Some(dst) = direct_result {
                self.emit_mov_reg_reg64(dst, RAX);
            }
        }
        for &patch in &done_patches {
            self.patch_rel32_to_here(patch);
        }
        if let Some(dst) = direct_result {
            self.publish_integer_result(dst, slot);
            if ref_node {
                self.ref_def_home_stores += 1;
            }
        } else {
            self.store_rax(slot);
        }
        if ref_node && !volatile_access {
            self.coestablish_loaded_reference(pc);
        }
        if !volatile_access {
            // A helper reached only through the implicit-null fault never
            // rejoins (a null receiver's helper call throws the NPE and leaves
            // through the sentinel exit), so it keeps older block-CSE entries.
            let had_slow_rejoin = slow_edges || load_rejoins;
            self.block_cse_record_field_load(
                base,
                field_index as u32,
                node_ty,
                cell_off,
                type_tag,
                had_slow_rejoin,
            );
        }
    }

    /// Compare the receiver against an address in the read bounds table.
    pub(in crate::ir_lower) fn emit_cmp_receiver_mem_rdx(&mut self, receiver: u8, disp: i32) {
        use crate::x64::isel::{Mem, Op as MachineOp, Operand, Req, Ty};
        self.emit_field_instruction(Req::new(
            MachineOp::Cmp,
            Ty::I64,
            Operand::Gpr(receiver),
            Operand::Mem(Mem::base_disp32(RDX, i64::from(disp))),
        ));
    }

    /// Copy the (possibly rewritten) published values back into their frame
    /// slots and retract `top`. Pairs with [`Self::emit_shadow_push`].
    ///
    /// Emitted immediately after the call returns — deliberately BEFORE the
    /// result is stored. The single-pass backend uses RAX as its value temp,
    /// which would collide with the return value and force this after the
    /// store; RCX held an outgoing argument and is dead the instant the call
    /// returns, so using it keeps RAX untouched and lets this sit at the one
    /// choke point (`emit_call_return_check`) all dispatch routes share.
    pub(in crate::ir_lower) fn emit_shadow_reload(&mut self) {
        let offsets = std::mem::take(&mut self.pending_shadow);
        if self.emit_shadow_reload_words(&offsets) {
            self.shadow_reloads += 1;
        }
    }

    /// [`Self::emit_shadow_reload`]'s sequence for the published words
    /// `offsets`, without consuming `pending_shadow` or counting a reload.
    /// `false` (nothing emitted) when there is nothing to reload. JIT round 19
    /// wave 7 (lane handler7, HANDLER6-3): also the compiled handler probe's
    /// hit edge for an exit that left before its site's reload
    /// ([`Self::emit_catch_probes`]); that edge never reaches the site's own
    /// reload, so it retracts the publication itself, and the balance check
    /// (`shadow_pushes == shadow_reloads`) still counts the site's one reload.
    /// Clobbers RCX, R10 and R11 only (RAX is preserved).
    pub(in crate::ir_lower) fn emit_shadow_reload_words(&mut self, offsets: &[i16]) -> bool {
        if self.get_current_thread == 0
            || self.shadow_thread_slot_off <= 0
            || self.shadow_savebase_slot_off <= 0
            || offsets.is_empty()
        {
            return false;
        }
        let ss_top = self.shadow_off_in_thread;
        // MOV R10, [rbp - thread_slot] ; TEST ; JE skip  (mirrors the push)
        self.buf.emit(&[0x4C, 0x8B, 0x95]);
        self.buf.emit(&(-self.shadow_thread_slot_off).to_le_bytes());
        self.buf.emit(&[0x4D, 0x85, 0xD2]);
        self.buf.emit(&[0x0F, 0x84]);
        let skip = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        // MOV R11, [rbp - savebase]    (restore from THIS push's base, not the
        // live `top`, so an unbalanced intervening push cannot drift it)
        self.buf.emit(&[0x4C, 0x8B, 0x9D]);
        self.buf
            .emit(&(-self.shadow_savebase_slot_off).to_le_bytes());
        // Overflow bail protocol (see `emit_shadow_push`): a tagged base means
        // the push stored nothing, so the restore below would read slots that
        // were never written. Skip to the tail, which only retracts `top`.
        self.buf.emit(&[0x49, 0xF7, 0xC3]); // TEST R11, imm32
        self.buf.emit(&1i32.to_le_bytes());
        self.buf.emit(&[0x0F, 0x85]); // JNE overflow
        let overflow = self.buf.pos();
        self.buf.emit(&[0, 0, 0, 0]);
        for &off in offsets {
            // MOV RCX, [R11] ; MOV [rbp - off], RCX ; LEA R11, [R11 + 8]
            self.buf.emit(&[0x49, 0x8B, 0x0B]);
            self.buf.emit(&[0x48, 0x89, 0x8D]);
            self.buf.emit(&(-(off as i32)).to_le_bytes());
            self.buf.emit(&[0x4D, 0x8D, 0x5B, 0x08]);
        }
        self.patch_rel32_to_here(overflow);
        // MOV R11, [rbp - savebase] ; AND R11, ~1 ; MOV [R10 + ss_top], R11
        // (retract top; the mask is a no-op unless the push bailed)
        self.buf.emit(&[0x4C, 0x8B, 0x9D]);
        self.buf
            .emit(&(-self.shadow_savebase_slot_off).to_le_bytes());
        self.buf.emit(&[0x49, 0x83, 0xE3, 0xFE]);
        self.buf.emit(&[0x4D, 0x89, 0x9A]);
        self.buf.emit(&ss_top.to_le_bytes());
        self.patch_rel32_to_here(skip);
        true
    }

    /// The shadow description a cold path hands to
    /// [`crate::runtime_lowering::emit_shadow_copy_back`], or `None` when this
    /// site published nothing (or the shadow stack is not wired).
    pub(in crate::ir_lower) fn pending_shadow_copy_back(&self) -> Option<(i32, i32, Vec<i16>)> {
        if self.get_current_thread == 0
            || self.shadow_thread_slot_off <= 0
            || self.shadow_savebase_slot_off <= 0
            || self.pending_shadow.is_empty()
        {
            return None;
        }
        Some((
            self.shadow_thread_slot_off,
            self.shadow_savebase_slot_off,
            self.pending_shadow.clone(),
        ))
    }

    /// Copy the pending shadow publication back into its frame homes without
    /// consuming it or retracting `top` — the early half of
    /// [`Self::emit_shadow_reload`] for a cold path that reads homes before the
    /// site's reload runs. Not counted in `shadow_reloads`: the site's one
    /// real reload still follows on every path, which is what that balance
    /// check is about.
    pub(in crate::ir_lower) fn emit_pending_shadow_copy_back(&mut self) {
        if let Some((thread_slot_off, savebase_slot_off, offsets)) = self.pending_shadow_copy_back()
        {
            crate::runtime_lowering::emit_shadow_copy_back(
                &mut self.buf,
                &crate::runtime_lowering::ShadowCopyBack {
                    thread_slot_off,
                    savebase_slot_off,
                    offsets: &offsets,
                },
            );
        }
        // Round 11 (irlower): the copy-back just refreshed the HOME words of
        // everything a collection during the call may have moved, and both
        // callers (the aastore arm's second helper call, the callee-deopt
        // service's re-staging) go on to read references through
        // `gp_load_value` INSIDE the same node -- before the node-end
        // invalidation in `lower_block` runs. A register copy made before the
        // call would be read in preference to the refreshed word. Compile-time
        // bookkeeping only (a no-op unless `CRATONVM_JIT_IR_REF_RESIDENCY` is
        // on), and conservative on both sides of whatever branch this sits in.
        self.invalidate_ref_residency(None);
    }

    /// Zero the frame slot of every `Ref`-typed phi, and mark those slots
    /// publishable.
    ///
    /// Phi slots are reserved by `prealloc_phi_slots` before any code runs and
    /// are written later, by the edge copy of whichever predecessor is taken.
    /// A safepoint reached on a path that has not yet written one would
    /// otherwise see the previous frame's leftovers through a slot the oop map
    /// claims is a live reference — the collector would then follow, and
    /// rewrite, a garbage word.
    ///
    /// Zeroing makes the unwritten state a null reference, which every root
    /// consumer already handles, so the whole phi set becomes safe to publish
    /// unconditionally. The alternative — proving per-safepoint which phis are
    /// written on every path reaching it — is a dataflow problem for a handful
    /// of stores. Only `Ref` phis are zeroed; a primitive phi read before its
    /// write would be a lowering bug, not a GC one, and zeroing it would hide
    /// that.
    pub(in crate::ir_lower) fn zero_ref_phi_slots(&mut self) {
        let refs: Vec<(usize, i32)> = (0..self.graph.nodes.len())
            .filter(|&id| {
                matches!(self.graph.nodes[id].op, Op::Phi) && self.graph.nodes[id].ty == IrType::Ref
            })
            // A phi with no location cannot be zeroed and must not be
            // published; `prealloc_phi_slots` gives every phi one, so this
            // filter drops nothing in a well-formed compile.
            .filter_map(|id| self.node_slot[id].map(|off| (id, off.get() as i32)))
            .collect();
        for (id, off) in refs {
            // MOV qword [rbp - off], 0 (or, in the prologue under CG16-8, the
            // RAX form of the same store).
            self.emit_zero_frame_slot(off);
            self.defined_nodes[id] = true;
        }
    }

    /// Zero every frame word a non-phi `Ref` value is coloured onto, before any
    /// code can reach a safepoint (round 9 wave 2).
    ///
    /// `emit_safepoint_map` publishes the slot of every `Ref` node in
    /// `defined_nodes`, which is EMISSION order: at a safepoint in the `else`
    /// arm of `if (c) { a = f(); } else { g(); }` the map names `a`'s slot,
    /// which on that path this activation never wrote. Only locals, the
    /// reserved words and `Ref` phis were zeroed, so that word held whatever an
    /// earlier frame left at the offset — and the collector, told it is a
    /// precise root, relocated or rewrote it
    /// (`ir-safepoint-maps-publish-ref-slots-this-path-never-wrote-20260918.md`).
    /// Zeroing makes the unwritten state a null reference, which every root
    /// consumer already handles; exactly `zero_ref_phi_slots`' argument,
    /// extended from phis to every reference colour.
    ///
    /// The alternative, filtering the map by dominance, costs nothing at run
    /// time but unpublishes words the conservative band scan then sees as
    /// unexplained young pointers, refusing moving cycles; this keeps every
    /// published set exactly as it was and changes only what an unwritten word
    /// contains.
    ///
    /// Cost is one store per distinct reference colour. `entry_runs` (the
    /// method prologue) skips a colour whose every occupant is defined in the
    /// ENTRY block: the entry block dominates every safepoint, and a safepoint
    /// before the definition does not publish it, so such a word is always
    /// written before it is named. An OSR entry stub jumps past the entry
    /// block and passes `false`.
    ///
    /// Only when this body publishes maps at all, and `CRATONVM_JIT_IR_ZERO_REF_SLOTS=0`
    /// is the kill switch.
    pub(in crate::ir_lower) fn zero_ref_value_slots(&mut self, entry_runs: bool) {
        if self.sp_id_slot_off <= 0 || !self.reloc_emit_on {
            return;
        }
        let entry_block = self
            .schedule
            .node_to_block
            .get(self.graph.entry as usize)
            .copied();
        let mut offs: Vec<i32> = Vec::new();
        for (id, n) in self.graph.nodes.iter().enumerate() {
            // Phis: `zero_ref_phi_slots`. A `Param`'s node copy is written
            // in the entry block, so it falls under the entry refinement
            // below; an OSR stub (which skips that block) zeroes it too.
            if n.ty != IrType::Ref || matches!(n.op, Op::Phi | Op::Dead) {
                continue;
            }
            let Some(color) = self.slot_plan.node_color.get(id).copied().flatten() else {
                continue;
            };
            if entry_runs
                && entry_block.is_some()
                && self.schedule.node_to_block.get(id).copied() == entry_block
            {
                continue;
            }
            // `planned_slot_off`'s arithmetic, checked the same way; a colour
            // outside the spill band is not one `alloc_slot` would hand out.
            let Some(off) = i32::try_from(u64::from(color).saturating_mul(8))
                .ok()
                .and_then(|d| self.first_spill.checked_add(d))
            else {
                continue;
            };
            if off <= 0 || off > self.spill_cap_off {
                continue;
            }
            // JIT round 25 item 2, wave 3: a lazy word is zeroed by the
            // entry poll's slow path and written before every other GC point
            // (`plan_block_lazy_clear`), not here. In the prologue only: an
            // OSR entry stub enters past the writes and zeroes every word.
            if self.prologue_zero_stretch
                && i16::try_from(off).is_ok_and(|w| self.block_lazy_clear.binary_search(&w).is_ok())
            {
                continue;
            }
            offs.push(off);
        }
        offs.sort_unstable();
        offs.dedup();
        for off in offs {
            self.emit_zero_frame_slot(off);
        }
    }

    /// `TEST BYTE [rip+disp32], 0xFF` against `safepoint_flag_addr` — the
    /// whole poll in one 7-byte instruction, reporting whether the flag was
    /// within ±2GB RIP reach of it.
    ///
    /// Reach only. Whether the short form is WANTED is
    /// `jit_rip_safepoint_poll_enabled()`, and the sole caller asks that first
    /// — see [`Self::emit_safepoint_poll`].
    ///
    /// Mirrors `x64/emit.rs`'s `emit_test_mem8_abs_imm8`; the two backends
    /// emit the same poll and this keeps them saying the same thing. `F6 /0 ib`
    /// with ModRM `mod=00, rm=101` is the RIP-relative form, and the
    /// displacement is measured from the end of the WHOLE instruction — past
    /// the trailing `imm8`, which is why the reach test adds 7 and not 6.
    ///
    /// The alternative it replaces, `MOV R11, imm64` + `TEST BYTE [R11], 0xFF`,
    /// is 15 bytes and two instructions and burns a register. Nothing here
    /// records a patch site: unlike the single-pass backend, this lowerer never
    /// duplicates emitted bytes to a second address, so a displacement that is
    /// right when emitted stays right.
    pub(in crate::ir_lower) fn emit_test_byte_rip(&mut self, addr: usize) -> bool {
        // F6 05 <disp32> <imm8>
        const LEN: usize = 7;
        // Cast: non-negative index/count to usize
        let here = self.buf.as_ptr() as usize + self.buf.pos();
        let Some(disp) = rip_disp32(here, LEN, addr) else {
            return false;
        };
        self.buf.emit(&[0xF6, 0x05]);
        self.buf.emit(&disp.to_le_bytes());
        self.buf.emit_byte(0xFF);
        true
    }

    pub(in crate::ir_lower) fn emit_test_safepoint_flag_rip(&mut self) -> bool {
        self.emit_test_byte_rip(self.safepoint_flag_addr)
    }

    /// JIT round 20 wave 20 (lane refspin; page
    /// `jr20-spliced-reference-get-loop-never-sees-the-clear-20261013.md`):
    /// before the back-edge poll of `back_edge_block`, `MOV qword [rbp - off],
    /// 0` for the home of every spliced `Reference.referent` read that is dead
    /// there ([`referent_loads_dead_at_back_edge`]).
    ///
    /// `emit_safepoint_map` publishes the home of every `Ref` node defined so
    /// far, live or not, and the frame block and band scan read the same word.
    /// A spliced `while (r.get() != null && n < limit) n++;` has no call in
    /// its body, so no armed dead-home site (`plan_dead_home_clears`) ever
    /// zeroes the read's home, and the back-edge poll -- the loop's only GC
    /// point -- handed the collector the last iteration's referent as a
    /// STRONG root at every collection: the referent was never cleared
    /// (`R20Rev16ReferentSpin`, every collector). HotSpot's oop map at that
    /// poll does not name the dead value. Unspliced, the `get()` call is an
    /// armed site, which is why `CRATONVM_JIT_IR_REFERENT_SPLICE=0` saw the
    /// clear. A correctness fix: unconditional; only zero is written, and only
    /// to a word no value needs at the poll.
    ///
    /// JIT round 20 wave 21 (lane pollroot; page
    /// `jr20-back-edge-poll-publishes-dead-reference-homes-20261013.md`):
    /// generalised. Under `CRATONVM_JIT_IR_BACK_EDGE_DEAD_HOME_CLEAR` (default
    /// on, [`Self::back_edge_dead_home_clear`]) the clears cover every `Ref`
    /// value dead at the poll ([`back_edge_dead_ref_homes`]): the referent
    /// reads are a subset, and an unspliced `while (r.get() != null)`'s call
    /// result, or the `x` of `x = h.get(); y = x.next;`, leave their homes
    /// there too, as HotSpot's precise oop map at the poll leaves them out.
    /// `=0` keeps the referent clears alone (unconditional). Each word is
    /// written once per poll however many dead values share it.
    pub(in crate::ir_lower) fn emit_back_edge_dead_home_clears(&mut self, back_edge_block: usize) {
        for off in self.back_edge_dead_home_clear_offsets(back_edge_block) {
            self.emit_dead_home_clear(off);
        }
    }

    /// The words [`Self::emit_back_edge_dead_home_clears`] zeroes at the poll
    /// of `back_edge_block`, computed at the poll (they depend on what has
    /// been lowered so far). Under [`ir_poll_slow_path_dead_home_clear_enabled`]
    /// the poll emits them in its slow path instead of its fast path.
    pub(in crate::ir_lower) fn back_edge_dead_home_clear_offsets(
        &mut self,
        back_edge_block: usize,
    ) -> Vec<i32> {
        let general = self.back_edge_dead_home_clear;
        if !general && self.graph.referent_load_pcs.is_empty() {
            return Vec::new();
        }
        let (graph, schedule, plan) = (self.graph, self.schedule, self.slot_plan);
        let dead = if general {
            // Follow-up: the exit rule's precise snapshot half, off the
            // dead-reference facts when this compile built them.
            let spliced = self.spliced_ranges;
            match self.dead_ref_clear.as_mut() {
                Some(facts) => {
                    let mut reach =
                        |pc: usize| facts.plain_reach_kept(resolve_spliced_pc(spliced, pc));
                    back_edge_dead_ref_homes_with(
                        graph,
                        schedule,
                        plan,
                        back_edge_block,
                        true,
                        Some(&mut reach),
                    )
                }
                None => back_edge_dead_ref_homes(graph, schedule, plan, back_edge_block, true),
            }
        } else {
            referent_loads_dead_at_back_edge(graph, schedule, plan, back_edge_block)
        };
        // Follow-up: the merge nodes of this latch's headers. A header phi is
        // zeroed only when it lives in its home alone -- no register of its
        // own (a coalesced back-edge source shares it, CG16-6) and no split
        // transitions that reload it.
        let header_ctrls: Vec<NodeId> = schedule
            .blocks
            .get(back_edge_block)
            .map(|blk| {
                blk.successors
                    .iter()
                    .copied()
                    .filter(|&h| h <= back_edge_block)
                    .filter_map(|h| schedule.blocks.get(h).map(|hb| hb.ctrl))
                    .collect()
            })
            .unwrap_or_default();
        let mut offs: Vec<i32> = Vec::new();
        for (value, _colour) in dead {
            // Not yet lowered (a block laid out after this one): its home was
            // never written by this body's code, and a later poll asks again.
            if !self.defined_nodes.get(value).copied().unwrap_or(false) {
                continue;
            }
            let header_phi = graph.nodes.get(value).is_some_and(|n| {
                matches!(n.op, Op::Phi)
                    && n.inputs
                        .as_slice()
                        .first()
                        .is_some_and(|m| header_ctrls.contains(m))
            });
            if header_phi
                && (self.gp_reg_of.get(value).copied().flatten().is_some()
                    || self.ls_managed.get(value).copied().unwrap_or(false)
                    || self.home_dropped.get(value).copied().unwrap_or(false))
            {
                continue;
            }
            let Some(off) = self
                .node_slot
                .get(value)
                .copied()
                .flatten()
                .and_then(|home| i32::try_from(home.get()).ok())
            else {
                continue;
            };
            // Inside the spill band only, as `emit_dead_ref_slot_clears`.
            if off < self.first_spill || off > self.spill_cap_off {
                continue;
            }
            if !offs.contains(&off) {
                offs.push(off);
            }
        }
        // JIT round 20 wave 21 (lane pollroot,
        // `jr20-inner-loop-poll-roots-dead-outer-loop-values-FIXED-20261013.md`):
        // values defined outside the loop, which no dominance argument
        // reaches, by the call-site rule at the poll.
        if general {
            for off in self.outer_dead_home_offsets(back_edge_block) {
                if !offs.contains(&off) {
                    offs.push(off);
                }
            }
        }
        if !offs.is_empty() && false {
            eprintln!(
                "[ir-slots] back-edge dead-home clears at block {back_edge_block}: offsets {offs:?}"
            );
        }
        offs
    }

    /// JIT round 20 wave 21 (lane pollroot,
    /// `jr20-inner-loop-poll-roots-dead-outer-loop-values-FIXED-20261013.md`): the
    /// homes, inside the spill band, of the colours dead at the back-edge
    /// poll of `latch` by the call-site rule
    /// ([`DeadRefSlotClear::dead_colours_at_back_edge_poll`]) whose every
    /// value is defined outside the loop ([`outer_ref_colours`]). Empty
    /// without the dead-reference facts, without a known poll position or
    /// bci, or off a reducible back edge.
    ///
    /// Why the call-site rule holds at a poll: the rule is "no value range
    /// covers the position and no frame state reachable from it names the
    /// colour"; the poll is a GC point with a known linear position (the
    /// terminator's, or the outgoing edge's) and a bci from which all code
    /// after it is reachable ([`back_edge_poll_bci`]), and the schedule-side
    /// half is recorded at the poll by the precise keep set's own backward
    /// liveness. A colour every value of which lives outside the loop is
    /// written by nothing inside it, so its word, once dead, is not
    /// re-filled until a definition outside runs again.
    pub(in crate::ir_lower) fn outer_dead_home_offsets(&mut self, latch: usize) -> Vec<i32> {
        let (graph, schedule, plan) = (self.graph, self.schedule, self.slot_plan);
        let Some(block) = schedule.blocks.get(latch) else {
            return Vec::new();
        };
        let headers: Vec<usize> = block
            .successors
            .iter()
            .copied()
            .filter(|&h| h <= latch)
            .collect();
        if headers.is_empty() || !headers.iter().all(|&h| schedule.dom.dominates(h, latch)) {
            return Vec::new();
        }
        let Some(pos) = back_edge_poll_pos(schedule, plan, latch) else {
            return Vec::new();
        };
        let Some(pc) = back_edge_poll_bci(graph, schedule, latch) else {
            return Vec::new();
        };
        let bci = resolve_spliced_pc(self.spliced_ranges, pc);
        let latch_ctrl = block.ctrl;
        let dead = match self.dead_ref_clear.as_mut() {
            Some(facts) => facts.dead_colours_at_back_edge_poll(pos, bci, latch_ctrl),
            None => return Vec::new(),
        };
        if dead.is_empty() {
            return Vec::new();
        }
        let excluded = |id: usize| {
            self.ls_managed.get(id).copied().unwrap_or(false)
                || (graph.nodes.get(id).is_some_and(|n| matches!(n.op, Op::Phi))
                    && self.gp_reg_of.get(id).copied().flatten().is_some())
        };
        let outer = outer_ref_colours(graph, schedule, plan, &headers, &excluded);
        let (first_spill, spill_cap_off) = (self.first_spill, self.spill_cap_off);
        dead.into_iter()
            .filter(|&c| outer.get(c as usize).copied().unwrap_or(false))
            .filter_map(|c| {
                let off = i32::try_from(u64::from(c).saturating_mul(8))
                    .ok()
                    .and_then(|delta| first_spill.checked_add(delta))?;
                (off > 0 && off >= first_spill && off <= spill_cap_off).then_some(off)
            })
            .collect()
    }

    /// Emit the default-on cooperative poll used at method entries and loop
    /// back-edges. Every reference keeps its home frame word (the collector's
    /// whole root set is frame offsets), so the no-argument slow path may be
    /// called directly; what the call clobbers of a register-resident
    /// PRIMITIVE is the residency plan's concern, not the poll's.
    ///
    /// Both gates come from `x64::licm` rather than being re-derived here, and
    /// that is the whole point of routing through them. This function used to
    /// parse `CRATONVM_JIT_SAFEPOINT_POLLS` inline — once per emitted poll site
    /// rather than once per process — and never looked at
    /// `CRATONVM_JIT_RIP_SAFEPOINT_POLL` at all, so the lever documented as
    /// "emit the pre-2026-09-02 form so the two encodings can be priced in one
    /// binary" reached only the single-pass backend. Measured 2026-09-10 on a
    /// hot counted loop: with the switch set, **392 of 394** poll sites still
    /// took the RIP form, because everything hot is compiled here and not
    /// there.
    ///
    /// `back_edge_block` is the block whose back edge this poll sits on, or
    /// `None` for the method-entry poll; see [`Self::emit_poll_fp_reloads`].
    ///
    /// `mode_exit_to` is the loop header an UNCONDITIONAL back edge jumps to
    /// (`None` for an `If` / `Switch` terminator, whose poll runs before the
    /// branch decides). Every edge of a branch heads a `Proj` block of its own
    /// here, and every validated layout puts it after the branch
    /// (`ir_schedule::validate_order`), so a conditional back edge polls in its
    /// taken edge's block — unconditionally; a terminator polls only in the
    /// unchecked creation-order layout (`CRATONVM_JIT_IR_RPO_LAYOUT=0`). In an OSR-tier
    /// compile such a poll's slow path ends with `TEST AL, 1; JNZ <pad>` (the
    /// innermost-frame bit; `TEST RAX, RAX` until wave 17):
    /// the verdict of `HelperFnSafepointSlowPath`, non-zero when the frame
    /// must run interpreted, leaves at the header with the state the back edge
    /// carries ([`Self::back_edge_mode_exit_state`]; interpreter round i1
    /// wave 15, lane L2). In an admitted method-entry compile (wave 18, lane
    /// L2) it is `TEST AL, 2` (`SAFEPOINT_VERDICT_POLLING_BODY`), and the
    /// poll names its body ([`Self::emit_poll_body_id_arg`]). The flag-clear
    /// path is byte-identical either way.
    pub(in crate::ir_lower) fn emit_safepoint_poll(
        &mut self,
        back_edge_block: Option<usize>,
        mode_exit_to: Option<usize>,
    ) {
        // JIT round 25 wave 6 (lane W6-B): the inspection spills this poll
        // owes in its SLOW path (empty unless `site_named_loop_on`). Taken
        // here, so a poll that is not emitted leaves none behind: nothing can
        // park a frame where no poll is.
        let slow_spills = std::mem::take(&mut self.poll_slow_spills);
        if !crate::x64::jit_safepoint_polls_enabled()
            || self.safepoint_flag_addr == 0
            || self.safepoint_slow_path == 0
            // j27 wave 2 (lane STRIPMINE): a strip-mined inner latch; its
            // outer loop polls once per strip.
            || back_edge_block.is_some_and(|b| {
                crate::ir_optimize::latch_enters_only_strip_inner(self.graph, self.schedule, b)
            })
        {
            self.fallthrough_poll_block = None;
            return;
        }
        // JIT round 25 wave 6 (W6-D): a leaf frame refuses any poll
        // (`verify_leaf_entry`).
        self.leaf_entry.polls_seen += 1;
        // JIT round 20 wave 20 (lane refspin): a spliced `Reference.referent`
        // read dead at this back edge leaves its home before the poll can
        // publish it; wave 21 (lane pollroot): so does every other `Ref` value
        // dead there (`emit_back_edge_dead_home_clears`).
        //
        // `CRATONVM_JIT_IR_POLL_SLOW_PATH_DEAD_HOME_CLEAR` (default on): the
        // words are computed here but zeroed in the slow path, the only way
        // into the safepoint whose map would publish them. Nothing else reads
        // a dead home -- the stale-set plan of the call sites
        // (`plan_dead_home_clears`) never counts a poll as cleaning -- so the
        // flag-clear path of every iteration stops paying one store per word
        // (23 per pass of `HttpHeaderValidationUtilTest`'s inner value loop).
        let slow_clears = match back_edge_block {
            Some(block) if ir_poll_slow_path_dead_home_clear_enabled() => {
                self.back_edge_dead_home_clear_offsets(block)
            }
            Some(block) => {
                self.emit_back_edge_dead_home_clears(block);
                Vec::new()
            }
            // JIT round 25 item 2, wave 3: the method-entry poll is the one GC
            // point the lazy words may reach unwritten (nothing of the body
            // has run); its slow path, the only way into its safepoint, zeroes
            // them (`plan_block_lazy_clear`).
            None => self
                .block_lazy_clear
                .iter()
                .map(|&w| i32::from(w))
                .collect(),
        };
        let ref_reloads =
            self.poll_ref_phi_reloads(back_edge_block.zip(mode_exit_to), &slow_clears);
        let kept_ref_regs = ref_reloads
            .iter()
            .fold(0u16, |mask, &(reg, _)| mask | (1 << reg));
        // Resolved at the poll, before its code: the state it describes is the
        // one every value is in right here, and the slow path restores it.
        let mode_exit = match (back_edge_block, mode_exit_to) {
            (Some(pred), Some(header)) => {
                let prev_mask = self.post_collect_ref_mask.replace(Some(kept_ref_regs));
                let outcome = self.back_edge_mode_exit_state(pred, header);
                self.post_collect_ref_mask.set(prev_mask);
                self.note_mode_exit_outcome(&outcome);
                if outcome.as_ref().is_ok_and(|fs| fs.caller.is_some()) {
                    // Wave 57 (lane L2): a loop inside a splice.
                    self.spliced_header_poll_exits += 1;
                }
                if outcome
                    .as_ref()
                    .is_ok_and(frame_state_names_a_virtual_object)
                {
                    self.poll_exits_naming_virtuals += 1;
                }
                // JIT round 19 wave 4 (lane osr4, OSR2-1): a promoted header's
                // exit carries its cells' write-backs. They were resolved by
                // the state above already; a second answer that disagrees
                // (it cannot: same inputs) drops the exit, fail closed.
                outcome.ok().and_then(|frame_state| {
                    self.promoted_cell_poll_writebacks(pred, header)
                        .ok()
                        .map(|cell_writebacks| PollModeExit {
                            frame_state,
                            cell_writebacks,
                        })
                })
            }
            _ => None,
        };
        // JIT round 21 lane stress (FUZZ21-1 / FUZZ21-7): under
        // `CRATONVM_DBG_JIT_DEOPT_A_LOT` / `CRATONVM_DBG_GC_AT_BACK_EDGE`, every
        // Nth arrival at this back edge skips the flag test(s) below with ZF
        // clear, so the `Jcc` that follows takes the slow path as if a pause
        // were requested. `None` (nothing emitted) with both switches off.
        let stress_jmp = if back_edge_block.is_some() {
            self.emit_poll_stress_countdown()
        } else {
            None
        };
        // Interpreter round i6 wave 119, lane L2 (`CRATONVM_JIT_POLL_THREAD_WORD`,
        // default OFF; stage 3 of `i24-L3`): the per-thread poll word through
        // its TLS mirror, the baked flag below when the mirror is null -- the
        // single-pass poll's shape (`x64::safepoint`). Both arms end on a
        // `TEST` whose ZF the poll's `JZ` / `JNZ` reads; the `JMP` keeps it.
        //
        // j27 wave 1 (lane LOOPCG, `CRATONVM_JIT_IR_POLL_WORD_SELECT`, default
        // ON): the same two answers, chosen into R11 before ONE byte test
        // (`poll_word_select::poll_word_select_bytes`), so the mirror-set path
        // runs five instructions where the two-test form ran six.
        //
        // j27 wave 5 (lane UNROLLRES, `CRATONVM_JIT_IR_PAGE_POLL`, opt-in): a
        // back-edge poll with no mode exit, no countdown and no owed latch may
        // take the page form instead (`ir_page_poll`), which replaces every
        // flag test below: `page_site` is its faulting `TEST`.
        let page_site = self.emit_page_poll_prefix(back_edge_block.is_some(), mode_exit.is_some());
        let word_select = page_site.is_none()
            && crate::x64::jit_poll_thread_word_enabled()
            && poll_word_select::poll_word_select_enabled();
        if word_select {
            // Cast: the probe answers a 32-bit segment displacement.
            let disp = crate::x64::loop_poll_word_tls_disp() as u32;
            let here = self.buf.as_ptr() as usize + self.buf.pos();
            let bytes = poll_word_select::poll_word_select_bytes(
                here,
                crate::x64::inline_rbp_tls_segment_prefix(),
                disp,
                self.safepoint_flag_addr,
                crate::x64::jit_rip_safepoint_poll_enabled(),
            );
            self.buf.emit(&bytes);
        }
        let word_tested = if word_select || page_site.is_some() {
            None
        } else if crate::x64::jit_poll_thread_word_enabled() {
            // Cast: the probe answers a 32-bit segment displacement.
            let disp = crate::x64::loop_poll_word_tls_disp() as u32;
            self.buf
                .emit_byte(crate::x64::inline_rbp_tls_segment_prefix());
            self.buf.emit(&[0x4C, 0x8B, 0x1C, 0x25]); // MOV R11, <seg>:[disp32]
            self.buf.emit(&disp.to_le_bytes());
            self.buf.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
            self.buf.emit(&[0x0F, 0x84]); // JZ .global
            let to_global = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            self.buf.emit(&[0x41, 0xF6, 0x03, 0xFF]); // TEST byte ptr [R11], 0xff
            self.buf.emit_byte(0xE9); // JMP .tested
            let to_tested = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            let rel = self.buf.pos() as i32 - (to_global as i32 + 4);
            Self::patch_or_bail(&mut self.buf, to_global, rel);
            Some(to_tested)
        } else {
            None
        };
        if !word_select
            && page_site.is_none()
            && (!crate::x64::jit_rip_safepoint_poll_enabled()
                || !self.emit_test_safepoint_flag_rip())
        {
            // The kill switch is set, or the flag is out of ±2GB RIP reach —
            // materialize the address and read through it, the shape this poll
            // had before 2026-09-02.
            //
            // t1gate (page `t1-D-proposal-gate-bytes-in-rip-reach-of-the-code-cache`):
            // the census line the page asks for; see the single-pass twin.
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] ir safepoint poll flag rip=false \
                     (out of disp32 reach or CRATONVM_JIT_RIP_SAFEPOINT_POLL=0)"
                );
            }
            self.emit_mov_reg_imm64(R11, self.safepoint_flag_addr as u64);
            self.buf.emit(&[0x41, 0xF6, 0x03, 0xFF]); // TEST byte ptr [R11], 0xff
        }
        if let Some(to_tested) = word_tested {
            let rel = self.buf.pos() as i32 - (to_tested as i32 + 4);
            Self::patch_or_bail(&mut self.buf, to_tested, rel);
        }
        // gcg g4/oo hunk 5.2 (b) 2, applied by g4/jm: under
        // `CRATONVM_GEN_CONC_OWED_POLL` (`DirectHelperTable::gen_owed_poll_flag_addr`
        // non-zero) the poll also tests the VM's owed-cycle latch, and either
        // byte set takes the slow path below. When outlined, `stw_set` is
        // filed under `outlined_poll_stw_patches` so `emit_outlined_polls`
        // patches both `JNZ .slow` jumps to the same slow block.
        let owed_latch = self.direct_helpers.gen_owed_poll_flag_addr;
        let stw_set = if owed_latch != 0 {
            let stw_set = if page_site.is_none() {
                self.buf.emit(&[0x0F, 0x85]); // JNZ .slow (STW flag set)
                let stw_set = self.buf.pos();
                self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                Some(stw_set)
            } else {
                None
            };
            if !crate::x64::jit_rip_safepoint_poll_enabled() || !self.emit_test_byte_rip(owed_latch)
            {
                self.emit_mov_reg_imm64(R11, owed_latch as u64);
                self.buf.emit(&[0x41, 0xF6, 0x03, 0xFF]); // TEST byte ptr [R11], 0xff
            }
            stw_set
        } else {
            None
        };
        // FUZZ21-1 / FUZZ21-7: the forced arrival lands here, on the `Jcc`,
        // with ZF clear.
        if let Some(jmp_disp) = stress_jmp {
            let rel = self.buf.pos() as i64 - (jmp_disp as i64 + 1);
            self.buf.patch_rel8_or_bail(jmp_disp, rel);
        }
        // ── Out-of-line: the fast path does not branch at all ─────────────
        //
        // The polarity below (`JZ` over the slow path) makes the FAST path —
        // every execution but the ones that actually stop — the TAKEN branch,
        // and leaves the slow path's ~230 bytes sitting between the poll and
        // whatever follows it. On `probes/FieldLoop.java`'s `sum` that is most
        // of the loop: the body spans 412 bytes from header to back edge, 228
        // of them this block, and roughly 123 of them ever execute.
        //
        // Emitting the slow path after the body instead inverts the test, so
        // the hot path falls through and the loop's span collapses to what it
        // runs. Only the METHOD-ENTRY poll takes it: the switch that also
        // outlined back-edge polls (`CRATONVM_JIT_IR_POLL_OUTLINE`, default
        // off, +39 % on Iropt4 osr in round 13) was deleted in round 14 wave 2.
        //
        // The METHOD-ENTRY poll (`back_edge_block == None`: the only caller
        // passing it is `lower_inner`, after the prologue) is outlined by its
        // own default-on switch, [`ir_entry_poll_outline_enabled`]: it has no
        // FP value to reload and no mode exit, so the outlined block is the
        // inline one moved, and every call of every IR body stops paying a
        // taken `JZ` over ~40 bytes of slow path (round 13 wave 1, lane
        // callcost; F8-2 of `r12w8-callcost-fib-call-anatomy-FIXED-20261001.md`).
        //
        // Round 17 (CG16P-1): a BACK-EDGE poll outlines too, under its own
        // switch, [`ir_back_edge_poll_outline_enabled`]. What the round-13
        // all-polls switch lacked is carried here: the outlined block reloads
        // the back edge's FP values (`emit_poll_fp_reloads`, keyed by the
        // block recorded with the site) between the shadow reload and the
        // mode exit, the inline arm's order; every other per-site input of
        // the map is snapshotted as for the entry poll. A forced exit
        // (`not_entrant::forced_exit_poll_cc`) turns the `JNZ` into `JNO`,
        // always taken, so a withdrawn body still leaves through the slow path.
        let entry_poll = back_edge_block.is_none() && mode_exit.is_none();
        let outline = if entry_poll {
            ir_entry_poll_outline_enabled()
        } else {
            back_edge_block.is_some() && ir_back_edge_poll_outline_enabled()
        };
        if outline {
            IR_POLLS_OUTLINED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            IR_POLLS_LOCAL.with(|c| {
                let (o, i) = c.get();
                c.set((o + 1, i));
            });
            // `JNZ .slow` (outlined, after the body), or nothing for the page
            // form: its fault is the branch (j27 wave 5, `ir_page_poll`).
            let slow_patch =
                self.emit_outlined_poll_branch(page_site, owed_latch != 0, mode_exit.is_some());
            let resume = self.buf.pos();
            // W6-B: emitted by `emit_outlined_polls` with the slow clears.
            // Keyed by position: a stale entry of a rewound poll at the same
            // position is replaced or removed here, never inherited.
            if slow_spills.is_empty() {
                self.outlined_poll_spills.remove(&slow_patch);
            } else {
                self.outlined_poll_spills.insert(slow_patch, slow_spills);
            }
            if let Some(stw_patch) = stw_set {
                self.outlined_poll_stw_patches.insert(slow_patch, stw_patch);
            } else {
                self.outlined_poll_stw_patches.remove(&slow_patch);
            }
            self.fallthrough_poll_block = None;
            self.outlined_polls.push((
                slow_patch,
                resume,
                self.spill_high_water,
                self.defined_nodes.clone(),
                self.cur_bci,
                mode_exit,
                self.cur_node_pc,
                back_edge_block,
                slow_clears,
                ref_reloads,
                self.gp_reg_owner,
            ));
            self.invalidate_ref_residency_except_regs(None, kept_ref_regs);
            return;
        }
        IR_POLLS_INLINE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        IR_POLLS_LOCAL.with(|c| {
            let (o, i) = c.get();
            c.set((o, i + 1));
        });
        // The page form skips with `JMP .clear`: its fault is the way in.
        self.buf.emit(ir_page_poll::inline_skip_opcode(
            page_site.is_some() && owed_latch == 0,
        )); // JZ .clear (or JMP .clear if page-only)
        let clear_patch = self.buf.pos();
        if mode_exit.is_some() {
            let exit_site = page_site.map_or(clear_patch, |s| s + 4);
            self.note_exit_poll_site(exit_site);
        }
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        if let Some(site) = page_site {
            // The recovery pc: the slow path's first byte.
            self.page_poll_sites.push((site, self.buf.pos()));
        }
        if let Some(stw_set) = stw_set {
            // `.slow`: the STW-flag `JNZ` above lands here.
            let rel = self.buf.pos() as i32 - (stw_set as i32 + 4);
            Self::patch_or_bail(&mut self.buf, stw_set, rel);
        }
        // The slow path can leave this frame (a mode exit, a deopt at the
        // safepoint), so under the fence sink it pays a deferred
        // volatile-store fence; the flag-clear path does not.
        if self.fence_sink_on() {
            self.buf.emit(&[0xF0, 0x83, 0x04, 0x24, 0x00]); // LOCK ADD DWORD [RSP], 0
        }
        // FUZZ21-3: the back-edge dead homes take the poison word under
        // `CRATONVM_DBG_DEAD_HOME_POISON`; the method-entry poll's lazy words
        // (never written) stay zero.
        for off in slow_clears {
            if back_edge_block.is_some() {
                self.emit_dead_home_clear(off);
            } else {
                self.emit_zero_frame_slot(off);
            }
        }
        // W6-B: the site-named values this poll's frame state names, into
        // their homes before anything can read the parked frame.
        for &(reg, off) in &slow_spills {
            self.store_abi_reg_raw(reg, off);
            self.poll_slow_spills_emitted += 1;
        }
        // THE POLL IS A SAFEPOINT, so it owes the relocation contract exactly
        // what a call owes it. It did not pay: no id was stored and no oop map
        // was recorded, so a thread parked in the slow path left its frame's
        // sp-id slot holding either the prologue's zero (no earlier GC point in
        // this frame -- the method-entry poll is ALWAYS this) or the id of some
        // EARLIER call, whose map describes a different program point.
        //
        // Both are refusals. `moving_young_frame_coverage_complete` finds no
        // `OopMapEntry` for the stored id, counts `NO_MAP_FOR_STORED_ID`, and
        // `frame_active_map_slots` / `moving_young_frame_live_hi` both return
        // `None` -- which is fail-closed by design, so every movable word in
        // the frame's band is reported unpublished and the whole cycle refuses
        // with `UNPUBLISHED_FRAME_OOP`. Measured on
        // `org.h2.test.jdbc.TestCachedQueryResults`, that is 100 % of the
        // `no-map-for-id` population and the reason its cross-thread handshake
        // reads `accepted=0 refused=1730`: one parked peer whose innermost
        // frame is at a poll refuses the collection for every thread.
        //
        // The single-pass backend has always bracketed its poll this way
        // (`x64/safepoint.rs::emit_safepoint_poll`: spill, sp-id store, call,
        // oop map). This is that same bracketing, in the IR backend's own
        // idiom -- it keeps every live value in a frame slot, so the map is a
        // set of frame offsets and there is no register file to spill.
        //
        // Emitted INSIDE the taken branch: the fast path (flag clear, which is
        // every execution but the ones that actually stop) is byte-for-byte
        // unchanged, so a back edge in a hot loop pays nothing.
        // gci i5/ir: an armed back-edge poll spends the dead-home facts
        // (`CRATONVM_JIT_IR_POLL_GC_POINT_CLEARS`); otherwise the plain map.
        let mut map = self.emit_poll_safepoint_map(back_edge_block);
        // Argument 1 = the body id, or none (waves 17/18): see
        // `emit_poll_body_id_arg`.
        self.emit_poll_body_id_arg();
        // Argument 0 = the flag byte this poll tested (names the VM).
        self.emit_mov_reg_imm64(CALL_ARG_REGS[0], self.safepoint_flag_addr as u64);
        self.emit_mov_reg_imm64(RAX, self.safepoint_slow_path as u64);
        self.buf.emit(&[0xFF, 0xD0]); // CALL RAX
        if map.is_some() {
            self.anchor_safepoint_return(map.as_mut(), self.buf.pos());
            // Pairs with the push `emit_safepoint_map` emitted: copies the
            // (possibly rewritten) published values back into their frame slots
            // and retracts `top`. Without it a poll that fires inside a loop
            // leaks its published oops and walks the 2 MiB shadow stack off its
            // end -- the failure mode the self-recursive direct-call arm
            // documents.
            self.emit_shadow_reload();
        }
        if let Some(block) = back_edge_block {
            self.emit_poll_fp_reloads(block);
        }
        // After all reloads, which leave RAX (the verdict) alone: the shadow
        // reload writes RCX/R10/R11 and the flags, the FP reload XMMs only, and
        // the ref-phi reloads write callee-saved GPRs only.
        self.emit_poll_ref_phi_reloads(&ref_reloads, map.as_ref());
        if let Some(exit) = mode_exit {
            self.emit_poll_mode_exit(exit);
        }
        let rel = self.buf.pos() as i32 - (clear_patch as i32 + 4);
        // IR safepoint poll -- tolerated on an overflowed buffer; see
        // `Self::patch_or_bail` / `patch_rel32_to_here`.
        Self::patch_or_bail(&mut self.buf, clear_patch, rel);
        // A slow return has refreshed the selected phi sources. Every other
        // reference copy may still hold a pre-relocation address.
        self.invalidate_ref_residency_except_regs(None, kept_ref_regs);
        self.fallthrough_poll_block = None;
    }

    /// Keep the resident references the back edge's phi copies read from
    /// authoritative homes. The slow path reloads only after GC copy-back.
    pub(in crate::ir_lower) fn poll_ref_phi_reloads(
        &self,
        edge: Option<(usize, usize)>,
        slow_clears: &[i32],
    ) -> Vec<(u8, i16)> {
        let Some((pred, succ)) = edge else {
            return Vec::new();
        };
        if !self.reg_alloc_on
            || !self.ref_residency_on
            || !self.phi_copy_regs_on
            || !self.reloc_emit_on
            || !Self::ir_gc_point_maps_enabled()
            || self.sp_id_slot_off <= 0
            || !ir_poll_slow_path_dead_home_clear_enabled()
            || self.poll_dead_ref_clear_site(pred).is_some()
        {
            return Vec::new();
        }
        let mut reloads = Vec::new();
        for copy in self.gather_phi_copies(pred, succ) {
            let id = copy.src;
            if copy.dst == copy.src_slot
                || self.graph.nodes[id as usize].ty != IrType::Ref
                || !self.defined_nodes[id as usize]
                || self.home_dropped.get(id as usize).copied().unwrap_or(false)
                || self.home_offset_of(id) != Some(copy.src_slot)
                || slow_clears.contains(&copy.src_slot)
            {
                continue;
            }
            let Some(reg) = self.resident_gpr(id) else {
                continue;
            };
            if !IR_LOWER_LS_GPRS.contains(&reg)
                || self.gp_reg_owner[reg as usize] != Some(id)
                || reloads.iter().any(|&(r, _)| r == reg)
            {
                continue;
            }
            if let Ok(home) = i16::try_from(copy.src_slot) {
                reloads.push((reg, home));
            }
        }
        self.extend_poll_invariant_ref_reloads(pred, slow_clears, &mut reloads);
        reloads
    }

    pub(in crate::ir_lower) fn emit_poll_ref_phi_reloads(
        &mut self,
        reloads: &[(u8, i16)],
        map: Option<&SafepointMapToken>,
    ) {
        if reloads.is_empty() {
            return;
        }
        let published = map
            .and_then(|token| self.oop_maps.get(token.index))
            .is_some_and(|entry| {
                reloads
                    .iter()
                    .all(|&(_, home)| entry.frame_slot_offsets.contains(&home))
            });
        if !published {
            self.latch_bailout(Bailout::new(BailoutReason::Internal(
                "IR poll phi reload is missing its authoritative reference home",
            )));
            return;
        }
        for &(reg, home) in reloads {
            self.load_reg_from_frame(reg, i32::from(home));
        }
    }

    /// JIT round 21 lane stress (FUZZ21-1 `CRATONVM_DBG_JIT_DEOPT_A_LOT`,
    /// FUZZ21-7 `CRATONVM_DBG_GC_AT_BACK_EDGE`): the per-site countdown in
    /// front of a back-edge poll's flag test. Returns the position of the
    /// `JMP rel8` displacement the caller patches to the poll's `Jcc`, or
    /// `None` -- having emitted nothing -- with both switches off.
    ///
    /// ```text
    ///   MOV  R11, &cell
    ///   LOCK SUB DWORD [R11], 1     ; ZF on the Nth arrival
    ///   JNZ  normal
    ///   MOV  DWORD [R11], N         ; re-arm
    ///   TEST RSP, RSP               ; ZF = 0: "a pause is requested"
    ///   JMP  jcc                    ; over the flag test(s)
    /// normal:
    ///   TEST BYTE [flag], 0xFF ...  ; the poll as before
    /// jcc:
    ///   JZ .clear / JNZ .slow
    /// ```
    ///
    /// Why this is sound wherever a poll is: the flags are dead at the poll's
    /// first byte (its own `TEST` overwrites them before the `Jcc` reads
    /// them), R11 is the per-opcode scratch that is never register-resident
    /// and that the poll's non-RIP form clobbers anyway, and the forced path
    /// reaches exactly the slow path a requested pause reaches, with the same
    /// published map and the same mode exit, so the state it hands the VM is
    /// the precise one the poll already describes. The VM tells the forced
    /// arrival apart by the flag byte being clear
    /// (`helpers::jit_safepoint_slow_path_body`). `LOCK` keeps two threads in
    /// one body from losing the zero; a racing re-arm only shifts the next
    /// forced arrival.
    pub(in crate::ir_lower) fn emit_poll_stress_countdown(&mut self) -> Option<usize> {
        let period = self.poll_stress_period;
        if period == 0 {
            return None;
        }
        let initial = poll_stress_initial_countdown(
            period,
            self.poll_stress_seed,
            self.poll_stress_cells.len(),
        );
        let cell = Box::new(std::sync::atomic::AtomicU32::new(initial));
        let cell_addr = cell.as_ref() as *const std::sync::atomic::AtomicU32 as u64;
        self.poll_stress_cells.push(cell);
        self.emit_mov_reg_imm64(R11, cell_addr);
        // LOCK SUB DWORD [R11], 1
        self.buf.emit(&[0xF0, 0x41, 0x83, 0x2B, 0x01]);
        // JNZ normal: over MOV (7) + TEST (3) + JMP rel8 (2).
        self.buf.emit(&[0x75, 0x0C]);
        self.buf.emit(&[0x41, 0xC7, 0x03]); // MOV DWORD [R11], imm32
        self.buf.emit(&period.to_le_bytes());
        self.buf.emit(&[0x48, 0x85, 0xE4]); // TEST RSP, RSP (ZF = 0)
        self.buf.emit(&[0xEB, 0x00]); // JMP rel8 -> the poll's Jcc (patched)
        Some(self.buf.pos() - 1)
    }

    /// JIT round 21 lane stress (FUZZ21-3, `CRATONVM_DBG_DEAD_HOME_POISON`):
    /// one dead-home clear. Off (the default) it is exactly
    /// [`Self::emit_zero_frame_slot`]. On, the word gets
    /// [`cratonvm_types::jit_activation::DEAD_HOME_POISON`] instead, as two
    /// `MOV dword [rbp - off (+4)], imm32` stores: like the zero store, they
    /// write no register and no flag, so the clear may still sit anywhere a
    /// clear sits. Only the VALUE changes; which words are cleared, and where,
    /// is the dead-home planners' business, untouched here. The prologue's
    /// zero-over-zero skip does not apply: the poison is not what a fresh
    /// frame holds.
    pub(in crate::ir_lower) fn emit_dead_home_clear(&mut self, off: i32) {
        // gkr (`T1W9ArgrefOwnedArgs temp-arg` on ZGC and G1): a parameter the
        // lazy plan took has its prologue word zeroed where its colour is.
        // The plan listed the planned site clears only, and a loop's entry
        // clear left the word, which every map names, set through the loop's
        // first allocation. Once the copy has run the word has no reader.
        let words: Vec<i32> = self
            .lazy_param_homes
            .iter()
            .filter(|&&(id, home, _)| {
                home == off
                    && self
                        .defined_nodes
                        .get(id as usize)
                        .copied()
                        .unwrap_or(false)
            })
            .map(|&(_, _, word)| word)
            .collect();
        for word in words {
            self.emit_dead_home_clear_word(word);
        }
        self.emit_dead_home_clear_word(off);
    }

    /// Emit planned clears before a node or its call fallback stages operands.
    /// XMM0 is this lowerer's operand scratch. Resident FP values occupy
    /// XMM2..XMM7. No GPR or condition flag is changed, and only the listed
    /// words (including lazy parameters' original homes) are written.
    pub(in crate::ir_lower) fn emit_dead_home_clear_batch(&mut self, offsets: &[i32]) {
        if !self.operand_in_place_on || self.dead_home_poison || self.mir_mode != MirMode::Off {
            for &off in offsets {
                self.emit_dead_home_clear(off);
            }
            return;
        }
        let mut words = offsets.to_vec();
        for &(id, home, original) in &self.lazy_param_homes {
            if offsets.contains(&home)
                && self
                    .defined_nodes
                    .get(id as usize)
                    .copied()
                    .unwrap_or(false)
            {
                words.push(original);
            }
        }
        words.sort_unstable();
        words.dedup();
        let mut stores = Vec::new();
        let mut i = 0;
        while i < words.len() {
            if words
                .get(i + 1)
                .is_some_and(|&next| words[i].checked_add(8) == Some(next))
            {
                stores.push((words[i + 1], true));
                i += 2;
            } else {
                stores.push((words[i], false));
                i += 1;
            }
        }
        if stores.len() + 1 >= words.len() {
            for &off in offsets {
                self.emit_dead_home_clear(off);
            }
            return;
        }
        self.buf.emit(&[0x66, 0x0F, 0xEF, 0xC0]); // PXOR XMM0, XMM0
        for (off, pair) in stores {
            if pair {
                self.emit_xmm_frame_move(XMM0, off, true);
            } else {
                self.emit_dead_home_clear_word(off);
            }
        }
    }

    /// One word of [`Self::emit_dead_home_clear`].
    pub(in crate::ir_lower) fn emit_dead_home_clear_word(&mut self, off: i32) {
        if !self.dead_home_poison {
            self.emit_zero_frame_slot(off);
            return;
        }
        let poison = cratonvm_types::jit_activation::DEAD_HOME_POISON;
        // Cast: the two 32-bit halves of the 64-bit poison word.
        let (lo, hi) = (poison as u32, (poison >> 32) as u32);
        self.buf.emit(&[0xC7, 0x85]); // MOV dword [rbp + disp32], imm32
        self.buf.emit(&(-off).to_le_bytes());
        self.buf.emit(&lo.to_le_bytes());
        self.buf.emit(&[0xC7, 0x85]);
        self.buf.emit(&(4 - off).to_le_bytes());
        self.buf.emit(&hi.to_le_bytes());
    }

    /// Record the poll whose `Jcc rel32` displacement starts at `patch` as one
    /// a redefinition that withdraws this body forces: its condition-code byte
    /// is `patch - 1` (`CompiledMethod::exit_poll_sites`; interpreter round i1
    /// wave 25, lane L6). Only a poll that names its body to the slow path
    /// (a non-zero compile id, [`Self::emit_poll_body_id_arg`]) can be told to
    /// leave, so only such a poll is recorded.
    pub(in crate::ir_lower) fn note_exit_poll_site(&mut self, patch: usize) {
        if self.compile_id.id() == 0 {
            return;
        }
        if let Some(cc) = patch.checked_sub(1).and_then(|cc| u32::try_from(cc).ok()) {
            self.exit_poll_sites.push(cc);
        }
    }

    /// Reload, inside a back-edge poll's slow path and after its `CALL`, every
    /// FP value `block`'s poll holds in a caller-saved XMM (round 11 wave 6).
    ///
    /// From the HOME word: `fp_store_value` writes it first at every definition
    /// and every edge copy goes through it, so it holds the value the register
    /// held before the call. That is what lets the model leave the XMM bank
    /// unclobbered at the poll (`ir_poll_preserves_fp`) without a save area.
    /// Only the slow path pays; the flag-clear path jumps over this.
    pub(in crate::ir_lower) fn emit_poll_fp_reloads(&mut self, block: usize) {
        let Some(ids) = self.poll_fp_reloads.get(&block) else {
            return;
        };
        let reloads: Vec<(NodeId, u8, bool)> = ids
            .iter()
            .filter_map(|&id| {
                // The ASSIGNED register (not `resident_xmm`): the reload is
                // what makes the value readable from it again after the call.
                let reg = self.assigned_xmm(id)?;
                let ty = self.graph.nodes.get(id as usize)?.ty;
                Some((id, reg, ty == IrType::Double))
            })
            .collect();
        for (id, reg, is_double) in reloads {
            // The PLANNED home, not `slot_of`: a value live across the edge
            // may be defined in a block the layout emits after this one, and
            // its home is the word its definition writes either way.
            match self.planned_slot_off(id) {
                Ok(off) => self.fp_load(reg, off, is_double),
                Err(bailout) => {
                    self.latch_bailout(bailout);
                    return;
                }
            }
        }
    }

    /// Is JVM local `slot` dead at this method's bytecode `bci` — not read on
    /// any path from there before it is written, exception edges included —
    /// by the liveness of [`PollExitBytecode`]? `false` whenever that cannot
    /// be answered (no bytecode handed over, a `bci` no block covers, a slot
    /// past `max_locals`), which keeps the value's own description.
    /// Interpreter round i1 wave 18, lane L2.
    pub(in crate::ir_lower) fn poll_exit_local_dead(&self, bci: usize, slot: usize) -> bool {
        // Round 15 wave 1 (lane sync, SS8-2): a self-locking body's exit frame
        // hands its monitor over only with local 0 naming the receiver
        // (`deopt::handed_method_monitor_index`), dead or not. Round 15 wave 3
        // (lane sync2, S15-3): an instance body's; a static one's local 0 is
        // an ordinary argument.
        if slot == 0 && self.self_lock.is_some() && self.self_lock_mirror_slot().is_none() {
            return false;
        }
        let Some(bytecode) = self.poll_exit_bytecode else {
            return false;
        };
        let liveness = self.poll_exit_liveness.get_or_init(|| {
            if bytecode.code_len > bytecode.code.len() {
                return None;
            }
            Some(crate::regalloc::live_locals_per_pc_all(
                bytecode.code,
                bytecode.code_len,
                bytecode.num_params,
                &[],
                bytecode.handlers,
                bytecode.num_locals,
            ))
        });
        liveness
            .as_ref()
            .is_some_and(|live| local_dead_at(live, bytecode.num_locals, bci, slot))
    }

    /// The interpreter frame an UNCONDITIONAL back edge `pred_block ->
    /// header_block` leaves with when its poll's verdict says the frame must
    /// run interpreted, or `None` to leave that poll verdict-blind as every
    /// poll of this tier was before (interpreter round i1 wave 15, lane L2;
    /// `i9-L5-jvmti-frames-already-compiled-finish-compiled`).
    ///
    /// # Why the header's state
    ///
    /// The snapshots that survive to lowering at a loop are the header's
    /// (`ea_ir_bridge::ir_prune_unconsumable_snapshots` keeps every `Merge` /
    /// `Region` bci and drops the `goto`'s), and the poll runs BEFORE the
    /// edge's φ copies, so the header's φs still hold the previous iteration.
    /// The state is therefore the header snapshot with each φ OF THIS HEADER
    /// replaced by its input on this edge — what the copies are about to
    /// write — resumed at the header (`REEXECUTE`), as if the jump had been
    /// taken. Nothing between the poll and the jump has an effect a resume
    /// could repeat or lose, and the interpreter's continuation is a state the
    /// graph reaches itself, so later re-entry assumes nothing it falsified.
    ///
    /// # Fail closed
    ///
    /// Every named value must be provably current at the poll: a constant, a
    /// parameter, this edge's input to a header φ (a node whose block
    /// dominates the edge's source), or a value whose block STRICTLY dominates
    /// the header (loop-invariant here). A non-φ must also have been emitted
    /// already. A φ of any other merge that does not dominate the header (the
    /// opaque OSR merge's, a stale snapshot's), a monitor, a spliced header and
    /// a snapshot at another bci all refuse. Values are resolved with the
    /// dropped-home latch QUIETED, so a value with no location comes back
    /// `Unsupported` instead of refusing the compile, and the frame must then
    /// pass `osr_exit::guard_exit_point_resumable`, which the planless
    /// transfer applies again; a register-described value also needs the
    /// deopt stub's register image.
    ///
    /// The refusal says why ([`ModeExitRefusal`]; interpreter round i1 wave
    /// 17, lane L1), for the census [`Self::note_mode_exit_outcome`] keeps.
    ///
    /// # Method-entry bodies (wave 18, lane L2)
    ///
    /// The same state serves a method-entry body ([`IrPollModeExits::MethodEntry`]):
    /// the stash sinks rebuild it as a NEW interpreter frame
    /// (`deopt_resume::resume_real_ir_deopt`, `helpers::try_resume_trapped_callee`,
    /// both through `build_deopt_frame_inner`) instead of writing it into a
    /// live one, which takes every frame `guard_exit_point_resumable` passes:
    /// single scope, `REEXECUTE` at the point's own bci, no monitor, no
    /// virtual object, no unresumable slot. Since wave 21 (lane L2) such a
    /// frame may also hold locks the compiled code took (`relock == false`,
    /// each in a frame word), which a new frame keeps held; since wave 23 also
    /// elided levels (`relock`) on an object in a frame word, which the new
    /// frame re-takes.
    pub(in crate::ir_lower) fn back_edge_mode_exit_state(
        &self,
        pred_block: usize,
        header_block: usize,
    ) -> Result<FrameState, ModeExitRefusal> {
        /// A site id above every node's: "the end of the back edge's block".
        const BACK_EDGE_SITE: NodeId = NO_NODE - 1;
        use ModeExitRefusal as R;
        // Each mode is gated on the switch of the sink its exit reaches: the
        // OSR door's planless transfer, or (wave 18) the stash sinks' precise
        // resume, which the door that chose `MethodEntry` already asked
        // (`ir_entry_mode_exits_admitted`).
        let sought = match self.poll_mode_exits {
            IrPollModeExits::Off => false,
            IrPollModeExits::OsrDoor => ir_osr_guard_exit_entries_enabled(),
            IrPollModeExits::MethodEntry => true,
            // Wave 53 (lane L2): only a poll that names its body can ever be
            // told to leave on the named bit.
            IrPollModeExits::MethodEntryNamedOnly => self.compile_id.id() != 0,
        };
        if !sought || !crate::deopt_real_enabled() {
            return Err(R::NotSought);
        }
        // Interpreter round i2 wave 56 (lane L2): the census lumps every
        // structural refusal under `shape`; under `CRATONVM_DBG_JITC=1` each
        // one also names its test (`note_ir_mode_exit_shape`), so a run can
        // rank them (shape 2 (b) of
        // `docs/feature-designs/jit-ir-optimizations.md`).
        let shape = |why: &'static str| {
            note_ir_mode_exit_shape(why, self.poll_mode_exits, &self.graph.scope_method_key);
            R::Shape
        };
        // JIT round 21 wave 3 (lane keepset, rev-ir finding 2): the shape
        // tests live in `back_edge_exit_shape`, which the keep set's latch
        // read (`spliced_latch_exit_reads`) calls too, so the two cannot drift.
        let (header_ctrl, header_bci) =
            back_edge_exit_shape(self.graph, self.schedule, pred_block, header_block)
                .map_err(&shape)?;
        let dom = &self.schedule.dom;
        // JIT round 19 wave 2 (lane osr2, FG16-5 / IROPS19-6,
        // `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE`): this loop keeps field cells in
        // registers and writes them back only at its exit, so memory is stale
        // at this poll. The interpreter resuming at the header would re-read
        // the stale cells: no exit here. Correctness, not policy: the loop
        // then runs to its exit compiled, as every refused poll's loop does.
        //
        // JIT round 19 wave 4 (lane osr4, OSR2-1,
        // `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_POLL_WRITEBACK`, opt-in): the
        // exit is given when every cell's write-back is describable at this
        // poll; its pad then stores each cell's value on this back edge before
        // the frame leaves (`Self::promoted_cell_poll_writebacks`,
        // `emit_cell_writeback`), so the interpreter resumes over exact memory.
        // A promoted loop inside a splice keeps the refusal.
        let promoted_header = self.graph.promoted_cell_header_bcis.contains(&header_bci);
        if promoted_header {
            if !ir_loop_field_promote_poll_writeback_enabled() {
                return Err(shape("promoted-field-cells"));
            }
            if self.pc_is_in_a_spliced_body(header_bci) {
                return Err(shape("promoted-field-cells-spliced"));
            }
            self.promoted_cell_poll_writebacks(pred_block, header_block)
                .map_err(&shape)?;
        }
        if self.pc_is_in_a_spliced_body(header_bci) {
            // Interpreter round i2 wave 57 (lane L2): a loop inside a spliced
            // callee leaves through a caller chain, under its stage's switch.
            return match self.spliced_header_chain_snapshot(header_bci) {
                Ok(si) => self.spliced_back_edge_mode_exit_state(
                    pred_block,
                    header_block,
                    header_ctrl,
                    si,
                ),
                Err(why) => Err(shape(why)),
            };
        }
        let sp = self
            .snapshot_published_by_site(&self.graph.nodes[header_ctrl as usize], header_bci)
            .and_then(|si| self.graph.safepoints.get(si))
            .ok_or(R::NoHeaderSnapshot)?;
        if sp.bci != header_bci {
            return Err(R::NoHeaderSnapshot);
        }
        // A header inside a `synchronized` block (interpreter round i1 wave
        // 21, lane L2; `i18-L2-ir-entry-exits-refused-for-monitor-methods`,
        // item (b)): only a METHOD-ENTRY exit, which the stash sinks rebuild
        // as a new frame through `build_deopt_frame_inner`. A lock the
        // compiled code took (`relock == false`) is left held there, and the
        // frame's own `monitorexit` releases it, as for any guard trap inside
        // the block.
        //
        // Since interpreter round i1 wave 25 (lane L2) an OSR-DOOR exit too:
        // its planless in-place transfer leaves each lock the frame holds
        // (`relock == false` — the compiled code's, or the interpreter's from
        // before the entry) with the live frame, and the door rewrites that
        // frame's record from the exit frame
        // (`osr_exit::poll_exit_point_resumable`, which the transfer asks
        // again). An elided level, which that transfer re-takes since wave
        // 26, is decided below.
        let holds_a_lock = !sp.monitors.is_empty();
        let lock_admitted = match self.poll_mode_exits {
            IrPollModeExits::MethodEntry | IrPollModeExits::MethodEntryNamedOnly => {
                IR_ENTRY_EXITS_HOLDING_A_LOCK_ENABLED
            }
            IrPollModeExits::OsrDoor => crate::osr_exit::OSR_POLL_EXITS_HOLDING_A_LOCK_ENABLED,
            IrPollModeExits::Off => false,
        };
        if holds_a_lock && !lock_admitted {
            return Err(shape("lock-at-the-header"));
        }
        // This edge's input to each of the header's value φs; an edge that
        // disagrees with itself (two inputs from one block) refuses.
        let mut on_edge: HashMap<NodeId, NodeId> = HashMap::new();
        for c in self.gather_phi_copies(pred_block, header_block) {
            if on_edge
                .insert(c.phi, c.src)
                .is_some_and(|prev| prev != c.src)
            {
                return Err(R::HeaderPhiInput);
            }
        }
        let block_of = |v: NodeId| -> Option<usize> {
            self.schedule
                .node_to_block
                .get(v as usize)
                .copied()
                .filter(|&b| b != usize::MAX)
        };
        let emitted = |v: NodeId| self.defined_nodes.get(v as usize).copied().unwrap_or(false);
        // Shape 2 (b) of the i41 page: an object escape analysis
        // scalar-replaced is no machine word, but the frame can name it as a
        // `VirtualObject` the exit re-allocates, as a guard's frame and the
        // post-call exits' do (`Self::post_call_exit_state`): javac's for-each
        // iterators live across the loop header (`Code.compressCatchTable`
        // refused its poll exits as `not_current` before).
        let virtual_object = |v: NodeId| {
            v != NO_NODE
                && crate::deopt::osr_guard_exit_virtuals_enabled()
                && self.sr_map.is_some_and(|sr| sr.objects.contains_key(&v))
        };
        let current = |v: NodeId| -> Result<NodeId, ModeExitRefusal> {
            if v == NO_NODE {
                return Ok(NO_NODE);
            }
            if virtual_object(v) {
                return Ok(v);
            }
            let node = self.graph.nodes.get(v as usize).ok_or(R::NotCurrent)?;
            if matches!(node.op, Op::Const(_) | Op::ConstF(_) | Op::Param(_)) {
                return Ok(v);
            }
            if node.op == Op::Phi && node.inputs.first() == Some(&header_ctrl) {
                // A φ of this header with no input on this edge refuses.
                let src = *on_edge.get(&v).ok_or(R::HeaderPhiInput)?;
                if virtual_object(src) {
                    return Ok(src);
                }
                let src_node = self.graph.nodes.get(src as usize).ok_or(R::NotCurrent)?;
                if matches!(src_node.op, Op::Const(_) | Op::ConstF(_) | Op::Param(_)) {
                    return Ok(src);
                }
                let b = block_of(src).ok_or(R::NotCurrent)?;
                let ready = src_node.op == Op::Phi || emitted(src);
                return if ready && dom.dominates(b, pred_block) {
                    Ok(src)
                } else {
                    Err(R::NotCurrent)
                };
            }
            let b = block_of(v).ok_or(R::NotCurrent)?;
            let dominates_header = b != header_block && dom.dominates(b, header_block);
            if node.op == Op::Phi && !dominates_header {
                // A φ of another merge: the opaque OSR merge's, a stale
                // snapshot's.
                return Err(R::ForeignPhi);
            }
            let ready = node.op == Op::Phi || emitted(v);
            if ready && dominates_header {
                Ok(v)
            } else {
                Err(R::NotCurrent)
            }
        };
        // A local the bytecode does not read again before writing it (dead at
        // the header, handlers included) is `Undefined` in the resumed frame
        // whatever node names it, since the interpreter cannot observe it:
        // what the single-pass snapshot publishes for such a slot (wave 18,
        // lane L2). Asked only for a slot that would otherwise refuse.
        let dead = |slot: usize| self.poll_exit_local_dead(header_bci, slot);
        if self.state_lost_a_value_to_the_optimizer(&sp.locals, &sp.stack, |slot| dead(slot)) {
            return Err(R::Unlocatable);
        }
        let mut local_nodes: Vec<NodeId> = Vec::with_capacity(sp.locals.len());
        for (slot, &v) in sp.locals.iter().enumerate() {
            match current(v) {
                Ok(n) => local_nodes.push(n),
                Err(_) if dead(slot) => local_nodes.push(NO_NODE),
                Err(refusal) => return Err(refusal),
            }
        }
        let mut stack_nodes: Vec<NodeId> = Vec::with_capacity(sp.stack.len());
        for &v in &sp.stack {
            stack_nodes.push(current(v)?);
        }
        // Each locked object must be current at the poll, as a local is.
        let mut monitor_nodes: HashMap<NodeId, NodeId> = HashMap::new();
        for &obj in &sp.monitors {
            monitor_nodes.insert(obj, current(obj)?);
        }
        // A primitive RESIDENT in its register right here is described by
        // that register, exclusive owner or not: the point is read at this
        // one native site, where the emitter's own reads trust the same
        // residency, and the slow path's CALL preserves every register of the
        // residency file. That keeps a loop counter whose home was dropped
        // (no pruned snapshot names it) describable. Anything else is
        // `frame_value_for`'s by-bci answer. Only with the stub's register
        // image (`deopt_regs_base`), which is what a register value is read from.
        //
        // A scalar-replaced object is described at the END of the back edge's
        // block, where the poll sits: everything in that block has run
        // (`frame_value_for_object` reads "earlier than the site" by node id,
        // and no node id reaches `BACK_EDGE_SITE`).
        let mut virtual_defined: std::collections::HashSet<NodeId> =
            std::collections::HashSet::new();
        let mut site_value = |n: NodeId| -> FrameValue {
            if let Some(sr) = self.sr_map.filter(|_| virtual_object(n)) {
                return self.frame_value_for_object(
                    n,
                    Some(pred_block),
                    Some((BACK_EDGE_SITE, BACK_EDGE_SITE)),
                    sr,
                    &mut virtual_defined,
                );
            }
            let resident = self.graph.nodes.get(n as usize).and_then(|node| {
                let primitive = matches!(node.ty, IrType::Int | IrType::Long);
                let reg =
                    if self.deopt_regs_base > 0 && primitive && !matches!(node.op, Op::Const(_)) {
                        // Only a register the (possibly sized) image holds.
                        self.resident_gpr(n)
                            .filter(|&r| self.deopt_image_holds_gpr(r))
                    } else {
                        None
                    };
                reg.map(|r| {
                    if node.ty == IrType::Long {
                        FrameValue::RegisterLong(r)
                    } else {
                        FrameValue::Register(r)
                    }
                })
            });
            resident.unwrap_or_else(|| self.frame_value_for(n))
        };
        let was_quiet = self.quiet_dropped_home.replace(true);
        let mut locals: Vec<FrameValue> = local_nodes.iter().map(|&n| site_value(n)).collect();
        let stack: Vec<FrameValue> = stack_nodes.iter().map(|&n| site_value(n)).collect();
        // A described object that could not be (a field with no single
        // reaching value, a store whose order is unknown) refuses, fail closed;
        // so does one in a frame holding a lock, which the OSR door's in-place
        // transfer does not re-allocate (`deopt::osr_in_place_virtuals_refusal`).
        let names_a_virtual = locals.iter().chain(stack.iter()).any(|v| {
            matches!(
                v,
                FrameValue::VirtualObject(_)
                    | FrameValue::VirtualObjectRef(_)
                    | FrameValue::ConditionalVirtualObject(_)
            )
        });
        if locals
            .iter()
            .chain(stack.iter())
            .any(|v| matches!(v, FrameValue::MaterializationRequired(_)))
            || (names_a_virtual && holds_a_lock)
        {
            self.quiet_dropped_home.set(was_quiet);
            return Err(R::Unlocatable);
        }
        let monitors = if holds_a_lock {
            self.resolve_monitors(sp, |obj| {
                monitor_nodes
                    .get(&obj)
                    .map_or(FrameValue::Unsupported, |&n| self.monitor_object_value(n))
            })
        } else {
            Vec::new()
        };
        self.quiet_dropped_home.set(was_quiet);
        // Every lock's object in a frame word; one with no location refuses
        // (fail closed). Interpreter round i1 wave 23, lane L2: an ELIDED
        // level (`relock`, the re-take half of a nested-lock elision's split,
        // or a lock escape analysis removed on an allocation it kept) is
        // admitted too. Every sink a method-entry exit reaches rebuilds the
        // frame through `build_deopt_frame_inner`, which enters such a level
        // `lock_depth` times on the object, exactly as it does for a guard
        // trap at the same point; a sink that refuses the frame releases only
        // the levels the compiled code took (`CompiledLocksOfAStash`). A
        // scalar-replaced object is not a frame word and still refuses. An
        // OSR-door exit (wave 25) writes the LIVE frame in place; since wave
        // 26 (lane L2) that transfer re-takes an elided level too
        // (`deopt_resume::transfer_osr_guard_exit_into_live_frame`), under
        // its own switch, which `osr_exit::poll_exit_point_resumable` reads.
        let elided_admitted = IR_ENTRY_EXITS_WITH_AN_ELIDED_LOCK_ENABLED
            && match self.poll_mode_exits {
                IrPollModeExits::MethodEntry | IrPollModeExits::MethodEntryNamedOnly => true,
                IrPollModeExits::OsrDoor => {
                    crate::osr_exit::OSR_POLL_EXITS_WITH_AN_ELIDED_LOCK_ENABLED
                }
                IrPollModeExits::Off => false,
            };
        if monitors.iter().any(|m| {
            (m.relock && !elided_admitted) || !matches!(m.object, FrameValue::StackSlotRef(_))
        }) {
            return Err(R::Unlocatable);
        }
        // A dropped home no longer in its register, for a dead local (above).
        for (slot, value) in locals.iter_mut().enumerate() {
            if matches!(value, FrameValue::Unsupported) && dead(slot) {
                *value = FrameValue::Undefined;
            }
        }
        let names_a_register = locals.iter().chain(stack.iter()).any(|v| {
            matches!(
                v,
                FrameValue::Register(_)
                    | FrameValue::RegisterLong(_)
                    | FrameValue::RegisterRef(_)
                    | FrameValue::XmmFloat(_)
                    | FrameValue::XmmDouble(_)
            )
        });
        if names_a_register && self.deopt_regs_base <= 0 {
            return Err(R::NoRegisterImage);
        }
        // OSR2-1 (lane osr4): the write-back pad calls a field helper, which
        // may clobber every caller-saved XMM; a frame naming an XMM register
        // would then hand the stub a clobbered word. Fail closed.
        if promoted_header
            && !ir_loop_field_promote_pad_xmm_save_enabled()
            && locals
                .iter()
                .chain(stack.iter())
                .any(|v| frame_value_names_xmm(v, 0))
        {
            // t1 wave 8 (lane PROMOTE): a scalar-replaced object's field
            // counts too (`frame_value_names_xmm`), as the XMM save covers it.
            return Err(shape("promoted-field-cells-xmm"));
        }
        // A value with no location at the poll (a dropped home not resident
        // in a register here), which `guard_exit_point_resumable` refuses.
        if locals
            .iter()
            .chain(stack.iter())
            .any(|v| matches!(v, FrameValue::Unsupported))
        {
            return Err(R::Unlocatable);
        }
        let bci = u32::try_from(header_bci).map_err(|_| shape("bci-out-of-range"))?;
        let reason = DeoptReason::OsrExit;
        let probe = DeoptimizationPoint {
            native_offset: 0,
            bci,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: crate::deopt::speculation_id(bci, reason),
            frame_state: FrameState {
                class_id: 0,
                method_key: String::new(),
                bci,
                locals,
                stack,
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::for_reason(reason),
        };
        // Asked without the monitors, which that predicate refuses for the
        // guard exits' sake; they were checked above and join the frame after.
        if crate::osr_exit::guard_exit_point_resumable(&probe) {
            let mut frame_state = probe.frame_state;
            frame_state.monitors = monitors;
            Ok(frame_state)
        } else {
            Err(R::NotResumable)
        }
    }

    /// JIT round 19 wave 4 (lane osr4, OSR2-1,
    /// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_POLL_WRITEBACK`): the write-backs a
    /// mode exit of the back edge `pred_block -> header_block` must perform
    /// before its frame leaves, one per field cell the loop field promotion
    /// (`ir_optimize::loop_field_forward::promote_loop_cells`) keeps in a
    /// register at this header. `Ok(empty)` at a header nothing was promoted
    /// at. `Err` names the `[ir-mode-exit-refused]` shape.
    ///
    /// # What is written, and why it is exact
    ///
    /// Each cell is found from its exit write-back store (recorded in
    /// `Graph::promoted_cell_writebacks` under this header's NODE):
    /// `If(Cmp(Ne, v, init)) -> Store(base.f = v)` (`LCmp(v, init) != 0` for
    /// a `long`), where `v` is the cell's value at the header and `init` the
    /// pre-header read. At the poll, `v` is its input on THIS back edge (the
    /// value the iteration that just ended left in the cell; the poll runs
    /// before the edge's copies, which is why the header state is resolved
    /// the same way), `init` and `base` are loop-invariant. The pad stores
    /// `v` when `v != init`, the exit's own condition and for the same JMM
    /// reason (never write a value this thread read back over another
    /// thread's write). The frame then LEAVES -- the shared deopt stub never
    /// returns into the body -- so no later exit compare can read an `init`
    /// the write-back made stale. The interpreter resumes at the header and
    /// reads exactly the memory the unpromoted loop would have left.
    ///
    /// # Fail closed
    ///
    /// The store must still have that shape, a constant field index, an
    /// `int` / `long` kind and a helper to call; `base` must be described by a
    /// reference frame word (read after the slow path's shadow reload, so a
    /// collection at this poll that moved the object is seen), `v` and `init`
    /// by a constant, a frame word or a residency register. Anything else
    /// refuses the exit, which leaves the poll exactly as it was before
    /// (verdict-blind; the loop finishes compiled).
    pub(in crate::ir_lower) fn promoted_cell_poll_writebacks(
        &self,
        pred_block: usize,
        header_block: usize,
    ) -> Result<Vec<CellWriteBack>, &'static str> {
        let header_ctrl = self
            .schedule
            .blocks
            .get(header_block)
            .ok_or("promoted-field-cells-no-header")?
            .ctrl;
        let Some(header_bci) = self
            .graph
            .nodes
            .get(header_ctrl as usize)
            .and_then(|n| n.bytecode_pc)
        else {
            return Ok(Vec::new());
        };
        if !self.graph.promoted_cell_header_bcis.contains(&header_bci) {
            return Ok(Vec::new());
        }
        if !ir_loop_field_promote_poll_writeback_enabled() {
            return Err("promoted-field-cells");
        }
        let stores: Vec<NodeId> = self
            .graph
            .promoted_cell_writebacks
            .iter()
            .filter(|&&(h, _)| h == header_ctrl)
            .map(|&(_, s)| s)
            .collect();
        if stores.is_empty() {
            // A copy of a promoted header, or a header whose bci another
            // loop's promotion recorded: no record of its cells.
            return Err("promoted-field-cells-unrecorded-header");
        }
        let mut on_edge: HashMap<NodeId, NodeId> = HashMap::new();
        for c in self.gather_phi_copies(pred_block, header_block) {
            if on_edge
                .insert(c.phi, c.src)
                .is_some_and(|prev| prev != c.src)
            {
                return Err("promoted-field-cells-phi-input");
            }
        }
        let dom = &self.schedule.dom;
        let block_of = |v: NodeId| -> Option<usize> {
            self.schedule
                .node_to_block
                .get(v as usize)
                .copied()
                .filter(|&b| b != usize::MAX)
        };
        let emitted = |v: NodeId| self.defined_nodes.get(v as usize).copied().unwrap_or(false);
        // The node holding `v`'s value at this poll (`back_edge_mode_exit_state`'s
        // `current`, without the virtual-object arm: a cell is never one).
        let current = |v: NodeId| -> Option<NodeId> {
            // j27 wave 5 (lane STRIPOSR2, `CRATONVM_JIT_IR_STRIP_MINE_OSR`): a
            // cell of a loop strip-mined in the OSR form is a phi of the inner
            // header; at this (outer) poll it is read through its outer twin.
            let v = crate::ir_optimize::strip_outer_twin(self.graph, v, header_ctrl).unwrap_or(v);
            let node = self.graph.nodes.get(v as usize)?;
            if matches!(node.op, Op::Const(_) | Op::Param(_)) {
                return Some(v);
            }
            if node.op == Op::Phi && node.inputs.first() == Some(&header_ctrl) {
                let src = *on_edge.get(&v)?;
                let src_node = self.graph.nodes.get(src as usize)?;
                if matches!(src_node.op, Op::Const(_) | Op::Param(_)) {
                    return Some(src);
                }
                let b = block_of(src)?;
                let ready = src_node.op == Op::Phi || emitted(src);
                return (ready && dom.dominates(b, pred_block)).then_some(src);
            }
            let b = block_of(v)?;
            let dominates_header = b != header_block && dom.dominates(b, header_block);
            let ready = node.op == Op::Phi || emitted(v);
            (ready && dominates_header).then_some(v)
        };
        let was_quiet = self.quiet_dropped_home.replace(true);
        let describe = |n: NodeId| -> Option<CellWriteBackSrc> {
            let node = self.graph.nodes.get(n as usize)?;
            if let Op::Const(c) = node.op {
                return Some(CellWriteBackSrc::Imm(c));
            }
            if matches!(node.ty, IrType::Int | IrType::Long) {
                if let Some(r) = self.resident_gpr(n) {
                    return IR_LOWER_LS_GPRS
                        .contains(&r)
                        .then_some(CellWriteBackSrc::Reg(r));
                }
            }
            match self.frame_value_for(n) {
                FrameValue::Int(c) | FrameValue::Long(c) => Some(CellWriteBackSrc::Imm(c)),
                FrameValue::StackSlot(off) | FrameValue::StackSlotLong(off) => {
                    Some(CellWriteBackSrc::Frame(off))
                }
                FrameValue::Register(r) | FrameValue::RegisterLong(r)
                    if IR_LOWER_LS_GPRS.contains(&r) =>
                {
                    Some(CellWriteBackSrc::Reg(r))
                }
                _ => None,
            }
        };
        let mut out: Vec<CellWriteBack> = Vec::with_capacity(stores.len());
        let mut result: Result<(), &'static str> = Ok(());
        for st in stores {
            let Some(cell) = self.promoted_cell_shape(st) else {
                // JIT round 20 wave 11 (lane pLoop, OSR4-2): a promoted
                // STATIC cell (`loop_field_forward::promote_loop_statics`):
                // the pad stores it with the inline `putstatic`'s two
                // stores, no helper and no base.
                if let Some(sc) = promoted_static_cell_shape_of(self.graph, st) {
                    let (Some(value), Some(init)) = (current(sc.value), current(sc.init)) else {
                        result = Err("promoted-field-cells-not-current");
                        break;
                    };
                    let (Some(value), Some(init)) = (describe(value), describe(init)) else {
                        result = Err("promoted-field-cells-unlocatable");
                        break;
                    };
                    let Some(target) =
                        self.static_cell_writeback_target(sc.class_id, sc.field_index, sc.long)
                    else {
                        result = Err("promoted-static-cells-not-direct");
                        break;
                    };
                    out.push(CellWriteBack {
                        base_off: 0,
                        field_index: i64::from(sc.field_index),
                        long: sc.long,
                        value,
                        init,
                        helper: 0,
                        static_cell: Some(target),
                    });
                    continue;
                }
                result = Err("promoted-field-cells-writeback-shape");
                break;
            };
            let helper = if cell.long {
                self.putfield_long
            } else {
                self.putfield_int
            };
            if helper == 0 {
                result = Err("promoted-field-cells-no-helper");
                break;
            }
            let (Some(base), Some(value), Some(init)) =
                (current(cell.base), current(cell.value), current(cell.init))
            else {
                result = Err("promoted-field-cells-not-current");
                break;
            };
            let FrameValue::StackSlotRef(base_off) = self
                .monitor_object_home(base)
                .unwrap_or_else(|| self.frame_value_for(base))
            else {
                result = Err("promoted-field-cells-base-not-a-frame-word");
                break;
            };
            let (Some(value), Some(init)) = (describe(value), describe(init)) else {
                result = Err("promoted-field-cells-unlocatable");
                break;
            };
            out.push(CellWriteBack {
                base_off,
                field_index: cell.field_index,
                long: cell.long,
                value,
                init,
                // Cast: a helper's address as the `MOV RAX, imm64` operand.
                helper: helper as u64,
                static_cell: None,
            });
        }
        self.quiet_dropped_home.set(was_quiet);
        result.map(|()| out)
    }

    /// JIT round 20 wave 11 (lane pLoop, OSR4-2): where a poll pad stores a
    /// promoted static cell -- the class's statics-base cell and the cell's
    /// two word offsets, exactly what [`Self::emit_inline_putstatic`] bakes
    /// for the same field. `None` (the pad is refused, the loop finishes
    /// compiled) unless the inline `putstatic` is on and the resolver still
    /// answers for the field (it answers monotonically once a class is
    /// initialised, so a cell the promotion took is answered here too).
    pub(in crate::ir_lower) fn static_cell_writeback_target(
        &self,
        class_id: u32,
        field_index: u32,
        long: bool,
    ) -> Option<StaticCellWriteBack> {
        if !inline_putstatic_enabled() {
            return None;
        }
        let base_cell = self
            .direct_helpers
            .resolve_static_base(class_id, field_index as usize)?;
        let (cell_off, _, hi_off) = static_cell_offsets(field_index)?;
        Some(StaticCellWriteBack {
            // Cast: the VM cell's address as the `MOV R11, imm64` operand.
            base_cell: base_cell as u64,
            cell_off,
            hi_off,
            long,
        })
    }

    /// The cell an exit write-back store of the loop field promotion names:
    /// `None` unless `store` is a live `Store(Int | Long)` with a constant
    /// field index, controlled by the taken edge of `If(Cmp(Ne, v, init))`
    /// (`If(Cmp(Ne, LCmp(v, init), 0))` for a `long`) where `v` is the value
    /// it stores -- the shape `promote_loop` builds.
    pub(in crate::ir_lower) fn promoted_cell_shape(
        &self,
        store: NodeId,
    ) -> Option<PromotedCellShape> {
        promoted_cell_shape_of(self.graph, store)
    }

    /// OSR2-3 (lane osr5): this compile's pending-store plan, computed once.
    pub(in crate::ir_lower) fn promoted_pending_plan(
        &self,
    ) -> Result<Option<&PromotedPending>, &'static str> {
        match self.promoted_pending.get_or_init(|| {
            let plan = plan_promoted_pending(self.graph, self.schedule);
            // OSR5-3 census (wave 7, lane osr7), once per compile.
            if let Ok(Some(p)) = &plan {
                if p.join_phis > 0 {
                    crate::ir_check_elim::note_promote_census(
                        crate::ir_check_elim::PROMOTE_CENSUS_JOIN_PHIS,
                        p.join_phis as u64,
                    );
                }
            }
            plan
        }) {
            Ok(plan) => Ok(plan.as_ref()),
            Err(why) => Err(why),
        }
    }

    /// OSR2-3: is `id` a kept, silent in-loop store of a promoted cell?
    pub(in crate::ir_lower) fn is_silent_promoted_store(&self, id: NodeId) -> bool {
        if self.graph.promoted_sunk_stores.is_empty() {
            return false;
        }
        matches!(self.promoted_pending_plan(), Ok(Some(plan)) if plan.silent.contains(&id))
    }

    /// OSR2-3: before node `id` is lowered, the write-backs its deopt pads
    /// carry. `None` (nothing to check afterwards) outside every promoted
    /// loop with kept stores. A plan the schedule refutes refuses the compile:
    /// the silent stores would otherwise be lost.
    pub(in crate::ir_lower) fn begin_promoted_pending(
        &mut self,
        id: NodeId,
    ) -> Option<PromotedPendingMark> {
        if self.graph.promoted_sunk_stores.is_empty() {
            return None;
        }
        let states = match self.promoted_pending_plan() {
            Err(why) => {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnsupportedShape("promoted field cells: pending-store plan"),
                    format!("n{id}: {why}"),
                ));
                return None;
            }
            Ok(None) => return None,
            Ok(Some(plan)) => {
                if plan.silent.contains(&id) {
                    return None;
                }
                plan.at_node.get(&id)?.clone()
            }
        };
        // A deopt inside a spliced body resumes at the enclosing invoke (or
        // publishes a chain), whose memory state is not the one tracked here.
        // Wave 7 (lane osr7): a guard's own frame bci is asked too -- the pc
        // the point is resolved at (`Op::Guard` builds its point from it), so
        // a guard whose node pc and frame bci disagree cannot slip a flat
        // spliced point past this test. Unconditional: it only refuses more.
        let spliced = self.graph.nodes.get(id as usize).is_some_and(|n| {
            let guard_bci = match n.op {
                Op::Guard { bci } => Some(bci),
                _ => None,
            };
            n.bytecode_pc
                .into_iter()
                .chain(guard_bci)
                .any(|pc| self.pc_is_in_a_spliced_body(pc))
        });
        // OSR5-1 (`..._PENDING_CHAIN`): a spliced node keeps its pad when every
        // point it publishes resumes at the callee's own bci (checked in
        // `end_promoted_pending`, which refuses a flat one).
        let chain_ok = spliced && ir_loop_field_promote_pending_chain_enabled();
        self.pad_writebacks = if spliced && !chain_ok {
            None
        } else {
            self.describe_pending_writebacks(id, &states)
        };
        self.pad_writebacks_attached = 0;
        Some(PromotedPendingMark {
            boxes: self.deopt_boxes.len(),
            rethrows: self.rethrow_pads.len(),
            spliced: chain_ok,
        })
    }

    /// OSR2-3: after node `id`: every deopt point it published must have
    /// taken a write-back pad, and none may name an XMM register (the
    /// helper call may clobber it). Otherwise the compile is refused.
    pub(in crate::ir_lower) fn end_promoted_pending(
        &mut self,
        id: NodeId,
        mark: Option<PromotedPendingMark>,
    ) {
        let Some(mark) = mark else {
            return;
        };
        let wbs = self.pad_writebacks.take();
        let new_boxes = self.deopt_boxes.len().saturating_sub(mark.boxes);
        if new_boxes == 0 && self.rethrow_pads.len() == mark.rethrows {
            return;
        }
        // OSR5-4 (lane osr6): with the pads saving the XMM homes, no refusal.
        // t1 wave 8 (lane PROMOTE, PROMOTE21-2): nor when every write-back of
        // the pad is a STATIC cell's -- `emit_static_cell_store` is two inline
        // stores through RAX / RCX / R11, no helper call, so it cannot clobber
        // an XMM register. Static cells reach the plan only under
        // `CRATONVM_JIT_IR_LOOP_PROMOTE_DEOPT_PADS` (default ON).
        let all_static = pad_writebacks_all_static(wbs.as_deref());
        // t1 wave 8 (lane PROMOTE): the WHOLE frame chain and every
        // scalar-replaced object's fields (`frame_named_xmms`, what the XMM
        // save itself covers), not only the innermost frame's locals and
        // stack: a chain point's caller frame (OSR5-1) or a virtual object's
        // field may name an XMM register the helper call clobbers too.
        let names_xmm = !ir_loop_field_promote_pad_xmm_save_enabled()
            && !all_static
            && self.deopt_boxes[mark.boxes..]
                .iter()
                .any(|p| !frame_named_xmms(&p.frame_state).is_empty());
        // OSR5-1 (lane osr7): a spliced node's points must all be chain points
        // (the callee's own frame over its callers); a flat one resumes at the
        // enclosing invoke and would re-run the stores the pad wrote back.
        let flat_in_splice = mark.spliced
            && self.deopt_boxes[mark.boxes..]
                .iter()
                .any(|p| p.frame_state.caller.is_none());
        let covered = wbs.is_some()
            && new_boxes == self.pad_writebacks_attached
            && self.rethrow_pads.len() == mark.rethrows
            && !names_xmm
            && !flat_in_splice;
        if !covered {
            self.latch_bailout(Bailout::with_context(
                BailoutReason::UnsupportedShape(
                    "promoted field cells: a deopt without a write-back",
                ),
                format!(
                    "n{id}: {new_boxes} point(s), {} written back, describable={}, xmm={names_xmm}, flat_in_splice={flat_in_splice}",
                    self.pad_writebacks_attached,
                    wbs.is_some()
                ),
            ));
        } else if mark.spliced {
            crate::ir_check_elim::note_promote_census(
                crate::ir_check_elim::PROMOTE_CENSUS_CHAIN_PADS,
                new_boxes as u64,
            );
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[ir] loop field promote: n{id}: {new_boxes} chain point pad(s) write back {} cell(s)",
                    wbs.map_or(0, |w| w.len())
                );
            }
        } else if crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] loop field promote: n{id}: {new_boxes} deopt pad(s) write back {} cell(s)",
                wbs.map_or(0, |w| w.len())
            );
        }
    }

    /// OSR2-3: describe each cell's write-back at the node about to be
    /// lowered, from the plan's `(exit store, current value)` rows. `None` when
    /// any operand is not describable here (an unknown value, a base not in a
    /// reference frame word, a value not yet defined), or a helper is missing.
    pub(in crate::ir_lower) fn describe_pending_writebacks(
        &self,
        at: NodeId,
        states: &[(NodeId, Option<NodeId>)],
    ) -> Option<Vec<CellWriteBack>> {
        let was_quiet = self.quiet_dropped_home.replace(true);
        let defined = |v: NodeId| {
            self.graph.nodes.get(v as usize).is_some_and(|n| {
                matches!(n.op, Op::Const(_) | Op::Param(_) | Op::Phi)
                    || self.defined_nodes.get(v as usize).copied().unwrap_or(false)
            })
        };
        let resident = std::cell::Cell::new(0u64);
        let describe = |n: NodeId| -> Option<CellWriteBackSrc> {
            if !defined(n) {
                return None;
            }
            if let Op::Const(c) = self.graph.nodes.get(n as usize)?.op {
                return Some(CellWriteBackSrc::Imm(c));
            }
            // OSR6-1 (lane osr7, `..._PENDING_RESIDENT`): the register first,
            // when the allocator's range proves it is the value's here.
            if let Some(r) = self.pending_resident_reg(n, at) {
                resident.set(resident.get() + 1);
                return Some(CellWriteBackSrc::Reg(r));
            }
            match self.frame_value_for(n) {
                FrameValue::Int(c) | FrameValue::Long(c) => Some(CellWriteBackSrc::Imm(c)),
                FrameValue::StackSlot(off) | FrameValue::StackSlotLong(off) => {
                    Some(CellWriteBackSrc::Frame(off))
                }
                FrameValue::Register(r) | FrameValue::RegisterLong(r)
                    if IR_LOWER_LS_GPRS.contains(&r) =>
                {
                    Some(CellWriteBackSrc::Reg(r))
                }
                _ => None,
            }
        };
        let mut out = Vec::with_capacity(states.len());
        let mut ok = true;
        for &(exit, cur) in states {
            let Some(cell) = promoted_cell_shape_of(self.graph, exit) else {
                // JIT round 21 (lane promote, TYPECHECK21-9,
                // `CRATONVM_JIT_IR_LOOP_PROMOTE_DEOPT_PADS`): a promoted
                // STATIC cell, stored as the poll pad stores it (the inline
                // `putstatic`'s two words, no helper, no base).
                let target = promoted_static_cell_shape_of(self.graph, exit).and_then(|sc| {
                    self.static_cell_writeback_target(sc.class_id, sc.field_index, sc.long)
                        .map(|t| (sc, t))
                });
                let Some((sc, target)) = target else {
                    ok = false;
                    break;
                };
                let Some(cur) = cur else {
                    ok = false;
                    break;
                };
                let (Some(value), Some(init)) = (describe(cur), describe(sc.init)) else {
                    ok = false;
                    break;
                };
                out.push(CellWriteBack {
                    base_off: 0,
                    field_index: i64::from(sc.field_index),
                    long: sc.long,
                    value,
                    init,
                    helper: 0,
                    static_cell: Some(target),
                });
                continue;
            };
            let helper = if cell.long {
                self.putfield_long
            } else {
                self.putfield_int
            };
            let (Some(cur), true) = (cur, helper != 0) else {
                ok = false;
                break;
            };
            let FrameValue::StackSlotRef(base_off) = self
                .monitor_object_home(cell.base)
                .unwrap_or_else(|| self.frame_value_for(cell.base))
            else {
                ok = false;
                break;
            };
            let (Some(value), Some(init)) = (describe(cur), describe(cell.init)) else {
                ok = false;
                break;
            };
            out.push(CellWriteBack {
                base_off,
                field_index: cell.field_index,
                long: cell.long,
                value,
                init,
                // Cast: a helper's address as the `MOV RAX, imm64` operand.
                helper: helper as u64,
                static_cell: None,
            });
        }
        self.quiet_dropped_home.set(was_quiet);
        if ok && resident.get() > 0 {
            crate::ir_check_elim::note_promote_census(
                crate::ir_check_elim::PROMOTE_CENSUS_RESIDENT_OPERANDS,
                resident.get(),
            );
        }
        ok.then_some(out)
    }

    /// OSR6-1 (lane osr7, `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_RESIDENT`):
    /// the register a deopt pad at node `at` may read the pending value `v`
    /// from, or `None` (then the existing description applies).
    ///
    /// Three facts make it the value's at the point, not merely its register:
    ///
    /// * `v` is an `int`/`long` the plan gave one register for its whole range
    ///   (not split-managed), currently published there (`resident_gpr`), and
    ///   that register is in the callee-saved IR file, so the pad's helper
    ///   calls and the point's own lowering leave it intact;
    /// * the allocator's live range of `v` STRICTLY contains `at`'s position.
    ///   The allocation is verified (`verify_allocation`): no other value
    ///   whose range covers a position of `v`'s range holds the register, and
    ///   a value defined AT `at` (whose range starts there) overlaps `v`'s, so
    ///   it holds another register. Strict at the top end too: at `v`'s last
    ///   use the node being lowered may reuse the register for its result
    ///   before its check jumps. `resident_gpr` alone is an emission-order
    ///   fact; the range is what makes it a fact on every path to `at`;
    /// * the round-16 back-edge coalescing (`coalesce_back_edge_phi_sources`)
    ///   puts a source in its phi's register only when no node after the
    ///   source in its block can deopt, so no pad runs while the register
    ///   holds the other one of the pair.
    pub(in crate::ir_lower) fn pending_resident_reg(&self, v: NodeId, at: NodeId) -> Option<u8> {
        if self.pending_ls_range.is_empty() || !ir_loop_field_promote_pending_resident_enabled() {
            return None;
        }
        let node = self.graph.nodes.get(v as usize)?;
        if !matches!(node.ty, IrType::Int | IrType::Long) {
            return None;
        }
        if self.ls_managed.get(v as usize).copied().unwrap_or(false) {
            return None;
        }
        let r = self.resident_gpr(v)?;
        if !IR_LOWER_LS_GPRS.contains(&r) {
            return None;
        }
        let p = self.pending_ls_pos.get(at as usize).copied().flatten()?;
        let (lo, hi) = self.pending_ls_range.get(v as usize).copied().flatten()?;
        (lo < p && p < hi).then_some(r)
    }

    /// The chain snapshot (`ir::SafepointSnapshot::is_guard_splice_state`)
    /// the builder recorded at the spliced loop header `header_pc` (a
    /// combined-buffer pc), when this compile's stage of the spliced-header
    /// poll exits is on: an OSR-door compile under
    /// [`crate::ir::ir_spliced_header_poll_exits_enabled`], a METHOD-ENTRY
    /// one under [`crate::ir::ir_entry_spliced_header_poll_exits_enabled`]
    /// whose artifact keeps some spliced source (checked per scope once the
    /// chain is built). `Err` names the refusal for the `[ir-mode-exit-refused]`
    /// line: `spliced-header` (the stage's switch is off, or the mode takes
    /// none: the refusal of waves 15-56), `spliced-header-no-kept-source` (a
    /// method-entry compile that keeps no spliced bytecode) or
    /// `spliced-header-no-chain-snapshot`. Interpreter round i2 wave 57, lane
    /// L2.
    pub(in crate::ir_lower) fn spliced_header_chain_snapshot(
        &self,
        header_pc: usize,
    ) -> Result<usize, &'static str> {
        let sought = match self.poll_mode_exits {
            IrPollModeExits::OsrDoor => crate::ir::ir_spliced_header_poll_exits_enabled(),
            IrPollModeExits::MethodEntry => {
                if !crate::ir::ir_entry_spliced_header_poll_exits_enabled() {
                    false
                } else if self.graph.entry_chain_source_keys.is_empty() {
                    return Err("spliced-header-no-kept-source");
                } else {
                    true
                }
            }
            // A named-only body's boxes must be flat poll exits of its own
            // frame (`CompiledMethod::named_poll_exits_only`).
            IrPollModeExits::MethodEntryNamedOnly | IrPollModeExits::Off => false,
        };
        if !sought {
            return Err("spliced-header");
        }
        let si = *self
            .splice_state_first_by_pc
            .get(&header_pc)
            .ok_or("spliced-header-no-chain-snapshot")?;
        match self.graph.safepoints.get(si) {
            Some(sp) if sp.bci == header_pc && sp.is_guard_splice_state() => Ok(si),
            _ => Err("spliced-header-no-chain-snapshot"),
        }
    }

    /// [`Self::back_edge_mode_exit_state`] for a loop header INSIDE a spliced
    /// callee (interpreter round i2 wave 57, lane L2; stage 1 and 2 of
    /// `docs/feature-designs/deopt-osr.md`):
    /// the caller CHAIN the back edge `pred_block -> header_block` leaves
    /// with -- the spliced callee at its own header bci (`REEXECUTE`), every
    /// caller parked in its `invoke` -- from the header's chain snapshot `si`.
    ///
    /// The flat path's argument, per value of every scope: the poll runs
    /// before the edge's φ copies, so each φ OF THIS HEADER is replaced by its
    /// input on this edge; any other value must be a constant, a parameter, or
    /// emitted in (or a φ of) a block strictly dominating the header. A
    /// caller's saved values are defined before its `invoke`, so they pass
    /// that test or refuse. The values are then resolved as the flat path
    /// resolves them (a resident primitive by its register, else
    /// `frame_value_for`, the dropped-home latch quieted), and the chain is
    /// linked as a guard's is ([`Self::link_splice_chain`]).
    ///
    /// Fail closed, beyond the flat path's tests: a monitor in the snapshot
    /// (a lock no spliced scope can name), a scalar-replaced object, a value
    /// with no location (only a local of the COMPILING method that is dead at
    /// its `invoke` may be `Undefined`; the callees' liveness is not
    /// computed here), a chain the VM would not materialise
    /// (`deopt::inlined_chain_refusal`), and, in a method-entry compile, a
    /// spliced scope whose bytecode the artifact does not keep
    /// ([`chain_scopes_have_kept_sources`]).
    pub(in crate::ir_lower) fn spliced_back_edge_mode_exit_state(
        &self,
        pred_block: usize,
        header_block: usize,
        header_ctrl: NodeId,
        si: usize,
    ) -> Result<FrameState, ModeExitRefusal> {
        let outcome =
            self.spliced_back_edge_mode_exit_state_inner(pred_block, header_block, header_ctrl, si);
        // Every refusal is named under `CRATONVM_DBG_JITC=1` (a structural
        // one already was), so a census run ranks the spliced loops' causes.
        if let Err(refusal) = &outcome {
            if let Some(why) = refusal.spliced_header_name() {
                note_ir_mode_exit_shape(why, self.poll_mode_exits, &self.graph.scope_method_key);
            }
        }
        outcome
    }

    /// The body of [`Self::spliced_back_edge_mode_exit_state`].
    pub(in crate::ir_lower) fn spliced_back_edge_mode_exit_state_inner(
        &self,
        pred_block: usize,
        header_block: usize,
        header_ctrl: NodeId,
        si: usize,
    ) -> Result<FrameState, ModeExitRefusal> {
        use ModeExitRefusal as R;
        let shape = |why: &'static str| {
            note_ir_mode_exit_shape(why, self.poll_mode_exits, &self.graph.scope_method_key);
            R::Shape
        };
        let sp = self.graph.safepoints.get(si).ok_or(R::NoHeaderSnapshot)?;
        let chain = sp.splice_scope.as_deref().ok_or(R::NoHeaderSnapshot)?;
        if !sp.monitors.is_empty() {
            return Err(shape("spliced-header-holds-a-lock"));
        }
        let n = chain.scopes.len();
        if n < 2 {
            return Err(shape("spliced-header-chain-names-no-caller"));
        }
        let mut on_edge: HashMap<NodeId, NodeId> = HashMap::new();
        for c in self.gather_phi_copies(pred_block, header_block) {
            if on_edge
                .insert(c.phi, c.src)
                .is_some_and(|prev| prev != c.src)
            {
                return Err(R::HeaderPhiInput);
            }
        }
        let dom = &self.schedule.dom;
        let block_of = |v: NodeId| -> Option<usize> {
            self.schedule
                .node_to_block
                .get(v as usize)
                .copied()
                .filter(|&b| b != usize::MAX)
        };
        let emitted = |v: NodeId| self.defined_nodes.get(v as usize).copied().unwrap_or(false);
        // The flat path's scalar-replaced objects (shape 2 (b) of the i41
        // page): a METHOD-ENTRY compile's chain, which every stash sink
        // rebuilds re-allocating such objects (`materialise_chain_virtuals`);
        // the OSR door's in-place chain transfer still refuses them.
        let virtual_object = |v: NodeId| {
            v != NO_NODE
                && self.poll_mode_exits.is_method_entry()
                && crate::deopt::osr_guard_exit_virtuals_enabled()
                && self.sr_map.is_some_and(|sr| sr.objects.contains_key(&v))
        };
        // The flat path's `current`.
        let current = |v: NodeId| -> Result<NodeId, ModeExitRefusal> {
            if v == NO_NODE {
                return Ok(NO_NODE);
            }
            if virtual_object(v) {
                return Ok(v);
            }
            let node = self.graph.nodes.get(v as usize).ok_or(R::NotCurrent)?;
            if matches!(node.op, Op::Const(_) | Op::ConstF(_) | Op::Param(_)) {
                return Ok(v);
            }
            if node.op == Op::Phi && node.inputs.first() == Some(&header_ctrl) {
                let src = *on_edge.get(&v).ok_or(R::HeaderPhiInput)?;
                if virtual_object(src) {
                    return Ok(src);
                }
                let src_node = self.graph.nodes.get(src as usize).ok_or(R::NotCurrent)?;
                if matches!(src_node.op, Op::Const(_) | Op::ConstF(_) | Op::Param(_)) {
                    return Ok(src);
                }
                let b = block_of(src).ok_or(R::NotCurrent)?;
                let ready = src_node.op == Op::Phi || emitted(src);
                return if ready && dom.dominates(b, pred_block) {
                    Ok(src)
                } else {
                    Err(R::NotCurrent)
                };
            }
            let b = block_of(v).ok_or(R::NotCurrent)?;
            let dominates_header = b != header_block && dom.dominates(b, header_block);
            if node.op == Op::Phi && !dominates_header {
                return Err(R::ForeignPhi);
            }
            let ready = node.op == Op::Phi || emitted(v);
            if ready && dominates_header {
                Ok(v)
            } else {
                Err(R::NotCurrent)
            }
        };
        // Where the COMPILING method's locals sit in the snapshot's stack
        // (`ir::SpliceScopeChain`'s layout: scope 0's operand stack, then each
        // caller's locals and operand stack, innermost caller first), and the
        // `invoke` it is parked in: the one scope whose liveness is known.
        let outer = chain.scopes.last().ok_or(R::NoHeaderSnapshot)?;
        let mut at = usize::try_from(chain.scopes[0].stack_len).map_err(|_| R::NoHeaderSnapshot)?;
        for s in &chain.scopes[1..n - 1] {
            at = at
                .checked_add(usize::try_from(s.locals_len).map_err(|_| R::NoHeaderSnapshot)?)
                .and_then(|a| a.checked_add(usize::try_from(s.stack_len).ok()?))
                .ok_or(R::NoHeaderSnapshot)?;
        }
        let outer_locals_len =
            usize::try_from(outer.locals_len).map_err(|_| R::NoHeaderSnapshot)?;
        let outer_locals = at..at.saturating_add(outer_locals_len);
        let outer_bci = usize::try_from(outer.bci).map_err(|_| R::NoHeaderSnapshot)?;
        let outer_dead = |stack_idx: usize| {
            outer_locals.contains(&stack_idx)
                && self.poll_exit_local_dead(outer_bci, stack_idx - outer_locals.start)
        };
        // The spliced scopes' liveness is unknown: every slot counts as live.
        if self.state_lost_a_value_to_the_optimizer(&sp.locals, &sp.stack, |_| false) {
            return Err(R::Unlocatable);
        }
        let mut local_nodes: Vec<NodeId> = Vec::with_capacity(sp.locals.len());
        for &v in &sp.locals {
            local_nodes.push(current(v)?);
        }
        let mut stack_nodes: Vec<NodeId> = Vec::with_capacity(sp.stack.len());
        for (i, &v) in sp.stack.iter().enumerate() {
            match current(v) {
                Ok(n) => stack_nodes.push(n),
                Err(_) if outer_dead(i) => stack_nodes.push(NO_NODE),
                Err(refusal) => return Err(refusal),
            }
        }
        if let Some(sr) = self.sr_map {
            if local_nodes
                .iter()
                .chain(stack_nodes.iter())
                .any(|&n| n != NO_NODE && sr.objects.contains_key(&n) && !virtual_object(n))
            {
                return Err(shape("spliced-header-virtual-object"));
            }
        }
        // The flat path's `site_value`: a resident primitive by its register,
        // only with the stub's register image; a scalar-replaced object at the
        // end of the back edge's block.
        const BACK_EDGE_SITE: NodeId = NO_NODE - 1;
        let mut virtual_defined: std::collections::HashSet<NodeId> =
            std::collections::HashSet::new();
        let mut site_value = |n: NodeId| -> FrameValue {
            if let Some(sr) = self.sr_map.filter(|_| virtual_object(n)) {
                return self.frame_value_for_object(
                    n,
                    Some(pred_block),
                    Some((BACK_EDGE_SITE, BACK_EDGE_SITE)),
                    sr,
                    &mut virtual_defined,
                );
            }
            let resident = self.graph.nodes.get(n as usize).and_then(|node| {
                let primitive = matches!(node.ty, IrType::Int | IrType::Long);
                let reg =
                    if self.deopt_regs_base > 0 && primitive && !matches!(node.op, Op::Const(_)) {
                        self.resident_gpr(n)
                            .filter(|&r| self.deopt_image_holds_gpr(r))
                    } else {
                        None
                    };
                reg.map(|r| {
                    if node.ty == IrType::Long {
                        FrameValue::RegisterLong(r)
                    } else {
                        FrameValue::Register(r)
                    }
                })
            });
            resident.unwrap_or_else(|| self.frame_value_for(n))
        };
        let was_quiet = self.quiet_dropped_home.replace(true);
        let locals: Vec<FrameValue> = local_nodes.iter().map(|&n| site_value(n)).collect();
        let mut stack: Vec<FrameValue> = stack_nodes.iter().map(|&n| site_value(n)).collect();
        self.quiet_dropped_home.set(was_quiet);
        if locals
            .iter()
            .chain(stack.iter())
            .any(|v| matches!(v, FrameValue::MaterializationRequired(_)))
        {
            return Err(R::Unlocatable);
        }
        for (i, value) in stack.iter_mut().enumerate() {
            if matches!(value, FrameValue::Unsupported) && outer_dead(i) {
                *value = FrameValue::Undefined;
            }
        }
        let names_a_register = locals.iter().chain(stack.iter()).any(|v| {
            matches!(
                v,
                FrameValue::Register(_)
                    | FrameValue::RegisterLong(_)
                    | FrameValue::RegisterRef(_)
                    | FrameValue::XmmFloat(_)
                    | FrameValue::XmmDouble(_)
            )
        });
        if names_a_register && self.deopt_regs_base <= 0 {
            return Err(R::NoRegisterImage);
        }
        if locals
            .iter()
            .chain(stack.iter())
            .any(|v| matches!(v, FrameValue::Unsupported))
        {
            return Err(R::Unlocatable);
        }
        let fs = self
            .link_splice_chain(chain, &locals, &stack, Vec::new())
            .map_err(|_| shape("spliced-header-chain-unpublishable"))?;
        if self.poll_mode_exits.is_method_entry()
            && !chain_scopes_have_kept_sources(&fs, &self.graph.entry_chain_source_keys)
        {
            return Err(shape("spliced-header-source-not-kept"));
        }
        let reason = DeoptReason::OsrExit;
        let probe = DeoptimizationPoint {
            native_offset: 0,
            bci: fs.bci,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: crate::deopt::speculation_id(fs.bci, reason),
            frame_state: fs,
            semantics: ResumeSemantics::for_reason(reason),
        };
        if !crate::osr_exit::guard_exit_point_resumable(&probe) {
            return Err(R::NotResumable);
        }
        if false {
            note_ir_spliced_header_exit_given(&probe.frame_state, self.poll_mode_exits);
        }
        Ok(probe.frame_state)
    }

    /// Did the optimizer drop a value this header state still needs
    /// ([`Self::optimizer_dropped_snapshot_values`])? True when a local that
    /// is not `dead` (per slot) or any operand reads `NO_NODE`, other than the
    /// upper half of a `long` / `double` whose lower half the slot before
    /// names. Always false for a compile the optimizer dropped nothing of.
    pub(in crate::ir_lower) fn state_lost_a_value_to_the_optimizer(
        &self,
        locals: &[NodeId],
        stack: &[NodeId],
        dead: impl Fn(usize) -> bool,
    ) -> bool {
        if !self.optimizer_dropped_snapshot_values {
            return false;
        }
        let wide = |v: NodeId| {
            self.graph
                .nodes
                .get(v as usize)
                .is_some_and(|n| matches!(n.ty, IrType::Long | IrType::Double))
        };
        snapshot_values_lost(locals, &dead, &wide) || snapshot_values_lost(stack, &|_| false, &wide)
    }

    /// Count what [`Self::back_edge_mode_exit_state`] decided for one
    /// unconditional back-edge poll: an exit given, or the refusal's cause, in
    /// this compile's tally (the `[ir-ls] poll mode exits` line) and the
    /// process totals of its tier ([`ir_poll_mode_exit_census`] for OSR-door
    /// compiles, [`ir_entry_poll_mode_exit_census`] since wave 18 for
    /// method-entry ones). Nothing is counted when no exit was sought
    /// (`NotSought`), so those compiles pay one match. Compile time only.
    /// Interpreter round i1 wave 17, lane L1
    /// (`i15-L2-ir-poll-mode-exit-coverage-has-no-census`).
    pub(in crate::ir_lower) fn note_mode_exit_outcome(
        &mut self,
        outcome: &Result<FrameState, ModeExitRefusal>,
    ) {
        let slot = match outcome {
            Ok(_) => 0,
            Err(refusal) => match refusal.cause_index() {
                Some(cause) => cause + 1,
                None => return,
            },
        };
        let row = if self.poll_mode_exits.is_method_entry() {
            CENSUS_METHOD_ENTRY
        } else {
            CENSUS_OSR_DOOR
        };
        self.mode_exit_census[slot] += 1;
        IR_POLL_MODE_EXIT_CENSUS[row][slot].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Interpreter round i2 wave 61, lane L2: every NON-structural cause
        // is named per site too (wave 56 named `shape` where it is decided),
        // so a census `not_current=2` can be attributed to its method.
        if slot > 1 {
            note_ir_mode_exit_cause(slot - 1, self.poll_mode_exits, &self.graph.scope_method_key);
        }
    }

    /// Inside a back-edge poll's slow path, after its reloads: `TEST AL,
    /// SAFEPOINT_VERDICT_INNERMOST_FRAME; JNZ <pad>` with an `OsrExit` point
    /// for `frame_state` ([`Self::back_edge_mode_exit_state`]). The
    /// innermost-frame bit, as the single-pass OSR poll tests it: an OSR body
    /// runs inside the frame that bit is about, and that frame is what the
    /// transfer sink asks about. (`TEST RAX, RAX` until interpreter round i1
    /// wave 17, when the other bit became about a body the poll names; an
    /// OSR-door poll names its body only since wave 23, for the VM's
    /// redefinition-withdrawal answer, [`Self::emit_poll_body_id_arg`].)
    /// The pad is emitted by [`Self::emit_poll_exit_pads`]; the
    /// flag-clear path never reaches this.
    ///
    /// In a method-entry compile (interpreter round i1 wave 18, lane L2) the
    /// test is `TEST AL, SAFEPOINT_VERDICT_POLLING_BODY` instead, as the
    /// single-pass method-entry poll tests it: such a body runs in no
    /// interpreter frame of its own, the innermost-frame bit is about some
    /// caller's, and the polling-body bit is about the body this poll names.
    pub(in crate::ir_lower) fn emit_poll_mode_exit(&mut self, exit: PollModeExit) {
        let PollModeExit {
            frame_state,
            cell_writebacks,
        } = exit;
        let Some(bit) = self.poll_mode_exits.verdict_bit() else {
            // `back_edge_mode_exit_state` gives no state when nothing is
            // sought, so this is not reached; the poll stays verdict-blind.
            return;
        };
        let reason = DeoptReason::OsrExit;
        let bci = frame_state.bci;
        let point = Box::new(DeoptimizationPoint {
            native_offset: self.buf.pos() as u32,
            bci,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: crate::deopt::speculation_id(bci, reason),
            frame_state,
            semantics: ResumeSemantics::for_reason(reason),
        });
        let point_ptr = point.as_ref() as *const DeoptimizationPoint as u64;
        self.poll_exit_boxes.push(point);
        self.buf.emit(&[0xF6, 0xC0, bit]); // TEST AL, imm8
        self.buf.emit(&[0x0F, 0x85]); // JNZ <pad>
        let pad_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        if !cell_writebacks.is_empty() && crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[ir] loop field promote: header bci {bci}: poll exit writes back {} cell(s)",
                cell_writebacks.len()
            );
        }
        self.poll_exit_pads
            .push((pad_patch, point_ptr, cell_writebacks));
    }

    /// The pads of [`Self::emit_poll_mode_exit`]: `MOV DEOPT_ARG0, point; JMP
    /// <shared deopt stub>`, the shape of [`Self::emit_deopt_pads`]. Run AFTER
    /// `lower_inner` has read `osr_sentinel_free` and the guard / exception
    /// exit facts (these exits resume exactly and need no admission) and
    /// before `emit_deopt_stub` consumes `deopt_stub_patches`. The registers
    /// reach the stub as the slow path left them: callee-saved ones intact
    /// across its CALL, and neither `DEOPT_ARG0` nor RAX in the residency
    /// file.
    pub(in crate::ir_lower) fn emit_poll_exit_pads(&mut self) {
        for (pad_patch, point_ptr, cell_writebacks) in std::mem::take(&mut self.poll_exit_pads) {
            // Tolerated on an overflowed buffer; see `patch_rel32_to_here`.
            self.patch_rel32_to_here(pad_patch);
            // OSR2-1 (lane osr4): a promoted loop's cells reach memory before
            // the frame leaves. The registers are the slow path's: the
            // write-backs read their callee-saved residency registers and
            // frame words, clobber only RAX, R11 and the call's caller-saved
            // set, and the field helpers preserve every register a frame
            // state of this tier can name (`IR_LOWER_LS_GPRS`).
            // OSR5-4 (lane osr6): the frame's XMM homes around the helper
            // calls, under its switch (empty otherwise: the `-xmm` refusal).
            let xmms = if cell_writebacks.is_empty() {
                Vec::new()
            } else {
                self.pad_point_xmms(point_ptr, true)
            };
            emit_pad_xmm_save(&mut self.buf, &xmms);
            for wb in &cell_writebacks {
                emit_cell_writeback(&mut self.buf, wb);
            }
            emit_pad_xmm_restore(&mut self.buf, &xmms);
            self.emit_mov_reg_imm64(DEOPT_ARG0, point_ptr);
            self.buf.emit_byte(0xE9); // JMP rel32 → the shared deopt stub
            let jmp_patch = self.buf.pos();
            self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
            self.deopt_stub_patches.push(jmp_patch);
        }
    }

    // ── Post-call exit sites (interpreter round i1 wave 28, lane L6) ──────
    //
    // Stage 3 of
    // `docs/feature-designs/profile-guided-inlining.md`:
    // the optimizing tier's twin of the single-pass OSR compile's post-call
    // exit (`x64/op_invoke.rs::emit_post_call_exit_site`, wave 27). After a
    // real call of an OSR-door compile, a five-byte `NOP` inside one 8-aligned
    // word; a class redefinition that withdraws the body rewrites it into a
    // `JMP` to a stub (`JitCache::force_withdrawn_exit_polls`), which asks the
    // safepoint slow path for the verdict alone (`POST_CALL_EXIT_VERDICT_ONLY`,
    // every register preserved) and, on "leave", jumps to the shared deopt
    // stub with an `OsrExit` point at the invoke's SUCCESSOR -- a frame whose
    // thread was inside the call (a parked peer, the redefining thread itself)
    // then leaves as soon as the call returns, before the rest of its
    // iteration runs a splice of the redefined class's old bytecode.
    //
    // The frame state is the invoke's own snapshot with the call's arguments
    // popped and its result pushed ([`Self::post_call_exit_state`]): an invoke
    // reads and writes no local, so its locals are the successor's. Every value
    // that snapshot names is PINNED by `plan_slots` (a call can deopt, so
    // `ea_ir_bridge::ir_prune_unconsumable_snapshots` keeps the invoke's
    // snapshot, and `plan_slots` gives every value a kept snapshot names a
    // colour of its own), so no value defined between its last use and the
    // site can share its home -- the colouring hazard the proposal's point 3
    // named does not arise for these values. The call's result is described
    // right after the node stored it.

    /// Kill switch for the optimizing tier's post-call exit sites. `false`
    /// emits no site, no stub and no point: every compile is byte-identical
    /// to wave 27's.
    pub(in crate::ir_lower) const IR_POST_CALL_EXITS_ENABLED: bool = true;

    /// Kill switch for post-call exit sites in a METHOD-ENTRY compile
    /// ([`IrPollModeExits::MethodEntry`]; interpreter round i1 wave 38, lane
    /// L3, the i24-L6 page's "optimizing METHOD-ENTRY body" item). `false`
    /// restores the OSR-door-only rule of waves 28-37: every method-entry
    /// compile is then byte-identical to wave 37's.
    ///
    /// Why a method-entry body may take one where waves 27-29 held back: the
    /// cost they named is the caller-held direct `CALL` of a `synchronized`
    /// callee (`jit_bridge::caller_held_body_is_closed`), which refuses any
    /// body with a deopt box. But both of that route's predicates also require
    /// the body to CALL NOTHING (`wrapped_body_attempt_never_reruns`:
    /// `_jit_invoke_infos`, the direct-callee entries, cycle cells and inline
    /// caches all empty), and a site exists only after an `Op::Call`, whose
    /// info the artifact keeps (`ir_call_infos` -> `_jit_invoke_infos`). A body
    /// that gets a site was already refused there, so the new box changes no
    /// binding. The exit is the method-entry back-edge poll exit's (wave 18):
    /// the same verdict bit (`SAFEPOINT_VERDICT_POLLING_BODY`), the same
    /// `OsrExit` point shape, the same stash sinks, which rebuild a NEW frame
    /// at the point's bci through `build_deopt_frame_inner`.
    pub(in crate::ir_lower) const IR_ENTRY_POST_CALL_EXITS_ENABLED: bool = true;

    /// At most this many post-call exit sites per compile; past the cap a call
    /// keeps wave 27's behaviour (its frame leaves at the next exit-capable
    /// back edge).
    ///
    /// 64 until interpreter round i2 wave 54 (lane L2), on the ground that a
    /// graph has at most `ir::IR_MAX_INVOKES` (64) calls. That bounds the
    /// method's OWN invokes only (`scan.invoke_ops`); since wave 47 a real
    /// call inside a spliced body gets a site too, and the splices add up to
    /// `IR_INLINE_MAX_TOTAL_BYTES` (1300) bytes of bytecode, so a method near
    /// the invoke budget that splices callees with calls ran out of sites and
    /// its last calls kept no exit. 512 is above any graph the two budgets
    /// admit (64 own calls, and at most one call per three spliced bytes);
    /// each site costs a five-byte `NOP`, a stub and a point, all proportional
    /// to the call it follows.
    pub(in crate::ir_lower) const IR_POST_CALL_EXIT_SITES_MAX: usize = 512;

    /// After the call node `id` was lowered: emit a post-call exit site when
    /// [`Self::post_call_exit_state`] describes the frame the call returns
    /// into. Only in a compile whose polls read a verdict -- an OSR-door
    /// compile, or since wave 38 a method-entry one
    /// ([`Self::IR_ENTRY_POST_CALL_EXITS_ENABLED`]) -- that reserved a compile
    /// id (the stub names its body to the verdict). Emits nothing and records
    /// nothing otherwise.
    pub(in crate::ir_lower) fn maybe_emit_post_call_exit_site(&mut self, id: NodeId) {
        let mode_admitted = match self.poll_mode_exits {
            IrPollModeExits::OsrDoor => true,
            IrPollModeExits::MethodEntry => Self::IR_ENTRY_POST_CALL_EXITS_ENABLED,
            // Wave 53 (lane L2): its graph has no call, and its boxes must be
            // poll exits only (`CompiledMethod::named_poll_exits_only`).
            IrPollModeExits::MethodEntryNamedOnly | IrPollModeExits::Off => false,
        };
        if !Self::IR_POST_CALL_EXITS_ENABLED
            || !mode_admitted
            || self.compile_id.id() == 0
            || self.post_call_exits_pending.len() >= Self::IR_POST_CALL_EXIT_SITES_MAX
            || self.buf.overflowed()
        {
            return;
        }
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        let Op::Call { info_ptr } = node.op else {
            return;
        };
        let Some(frame_state) = self.post_call_exit_state(id, info_ptr) else {
            return;
        };
        self.emit_post_call_exit_site_for(frame_state);
    }

    /// After a `getstatic` / `putstatic` node `id` whose helper may have run
    /// `<clinit>` (its class not proven initialized, so the lowering took the
    /// helper route): the post-call exit site an invoke gets
    /// ([`Self::maybe_emit_post_call_exit_site`]), its frame built from the
    /// access's own snapshot ([`Self::post_static_helper_exit_state`]). A `<clinit>` is Java code
    /// that can redefine a class this body spliced (item 1 of
    /// `interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007`,
    /// 2026-10-07; probe `I24PostHelperRedefinition.java`).
    /// Under [`ir_post_static_helper_exits_enabled`] and the conditions of
    /// the invoke's site.
    pub(in crate::ir_lower) fn maybe_emit_post_static_helper_exit_site(&mut self, id: NodeId) {
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        let (class_id, is_load) = match node.op {
            Op::LoadStatic { class_id, .. } => (class_id, true),
            Op::StoreStatic { class_id, .. } => (class_id, false),
            _ => return,
        };
        let Some(pc) = node.bytecode_pc else {
            return;
        };
        if !ir_post_static_helper_exits_enabled()
            || self.direct_helpers.class_is_initialized(class_id)
        {
            return;
        }
        let mode_admitted = match self.poll_mode_exits {
            IrPollModeExits::OsrDoor => true,
            IrPollModeExits::MethodEntry => Self::IR_ENTRY_POST_CALL_EXITS_ENABLED,
            IrPollModeExits::MethodEntryNamedOnly | IrPollModeExits::Off => false,
        };
        if !Self::IR_POST_CALL_EXITS_ENABLED
            || !mode_admitted
            || self.compile_id.id() == 0
            || self.post_call_exits_pending.len() >= Self::IR_POST_CALL_EXIT_SITES_MAX
            || self.buf.overflowed()
        {
            return;
        }
        let Some(frame_state) = self.post_static_helper_exit_state(id, pc, is_load) else {
            return;
        };
        self.emit_post_call_exit_site_for(frame_state);
    }

    /// The interpreter frame a `getstatic` / `putstatic` at `pc` (node `id`)
    /// returns into after its helper, resumed at the next instruction
    /// (`REEXECUTE` there): the access's own snapshot -- the one a deopt AT
    /// the access reads, kept because the helper can raise -- with the loaded
    /// value pushed, or the stored one popped, exactly as an invoke's site
    /// pops its arguments and pushes its result
    /// ([`Self::post_call_exit_state`]). Fail closed: the compiling method's
    /// own code only, the access's own (or only) snapshot at its bci, and
    /// every check [`Self::post_site_frame_state`] makes.
    pub(in crate::ir_lower) fn post_static_helper_exit_state(
        &self,
        id: NodeId,
        pc: usize,
        is_load: bool,
    ) -> Option<FrameState> {
        let sought = match self.poll_mode_exits {
            IrPollModeExits::OsrDoor => ir_osr_guard_exit_entries_enabled(),
            IrPollModeExits::MethodEntry => true,
            IrPollModeExits::MethodEntryNamedOnly | IrPollModeExits::Off => false,
        };
        if !sought || !crate::deopt_real_enabled() || self.pc_is_in_a_spliced_body(pc) {
            return None;
        }
        let graph = self.graph;
        let bytecode = self.poll_exit_bytecode.as_ref()?;
        if !matches!(bytecode.code.get(pc).copied(), Some(0xb2 | 0xb3)) {
            return None;
        }
        let next = pc + 3;
        if next >= bytecode.code_len {
            return None;
        }
        let bci = u32::try_from(next).ok()?;
        let node = graph.nodes.get(id as usize)?;
        let own = node
            .frame_snapshot
            .and_then(|si| graph.safepoints.get(si as usize))
            .filter(|sp| sp.bci == pc);
        let sp = match own {
            Some(sp) => sp,
            None => {
                if graph
                    .safepoints
                    .iter()
                    .filter(|sp| sp.bci == pc && !sp.is_splice_state())
                    .count()
                    != 1
                {
                    return self
                        .post_call_exit_refused(pc, "no single snapshot at the static access");
                }
                self.safepoint_first_by_bci
                    .get(&pc)
                    .and_then(|&si| graph.safepoints.get(si))?
            }
        };
        if sp.bci != pc {
            return None;
        }
        // `getstatic` consumes nothing and pushes the value; `putstatic`
        // consumes the value it stored.
        let keep = if is_load {
            sp.stack.len()
        } else {
            sp.stack.len().checked_sub(1)?
        };
        self.post_site_frame_state(id, pc, next, bci, sp, keep, is_load.then_some(id))
    }
}
