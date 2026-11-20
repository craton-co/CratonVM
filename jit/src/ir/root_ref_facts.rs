// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Constant root reference metadata (`ConstRefMeta`) for `Op::IndyRootRef`.

use crate::ir::{Graph, IrBuilder, IrType, NodeId, Op, STATIC_FINAL_OBJECT_FILED};

/// Metadata associated with a constant GC root slot (`Op::IndyRootRef { slot_addr }`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConstRefMeta {
    /// True when the slot holds a non-null object or array reference.
    pub non_null: bool,
    /// Exact VM class id when statically known (never `0` or `STATIC_FINAL_OBJECT_FILED`).
    pub exact_class_id: Option<u32>,
    /// Primitive-array `KIND_TAGS` byte when the root ref is a 1-D primitive array.
    pub prim_array_tag: Option<u8>,
    /// Known array length when the root ref is a trusted `static final` array.
    pub array_length: Option<i32>,
    /// Originating `static final` `(class_id, field_index)` key when emitted from `getstatic`.
    pub static_field: Option<(u32, u32)>,
}

/// Record non-null / exact-class / originating static-field facts for `slot_addr`.
pub(crate) fn record_const_ref_meta(
    graph: &mut Graph,
    slot_addr: usize,
    cid: u32,
    static_field: Option<(u32, u32)>,
    force_non_null: bool,
) {
    if slot_addr == 0 {
        return;
    }
    let exact = (cid != 0 && cid != STATIC_FINAL_OBJECT_FILED).then_some(cid);
    let non_null = force_non_null || cid != 0;
    let entry = graph.const_ref_meta.entry(slot_addr).or_default();
    entry.non_null |= non_null;
    if entry.exact_class_id.is_none() {
        entry.exact_class_id = exact;
    }
    if entry.static_field.is_none() {
        entry.static_field = static_field;
    }
}

impl IrBuilder {
    /// Emit `Op::IndyRootRef` for an initialized non-volatile `static final`
    /// object (`b'L'`) or array (`b'['`) field in `static_final_const_refs`
    /// and record its `ConstRefMeta` on `Graph`.
    pub(in crate::ir) fn try_emit_static_final_root_ref(
        &mut self,
        class_id: u32,
        field_index: u32,
    ) -> Option<NodeId> {
        let &(slot_addr, cid) = self
            .const_folds
            .static_final_const_refs
            .get(&(class_id, field_index))?;
        if slot_addr == 0 {
            return None;
        }
        let v = self.graph.add(
            Op::IndyRootRef { slot_addr },
            IrType::Ref,
            vec![self.ctrl],
            None,
        );
        self.const_ref_slots.insert(v, slot_addr);
        let exact_cid = if cid != 0 && cid != STATIC_FINAL_OBJECT_FILED {
            cid
        } else {
            self.static_final_exact_classes
                .get(&(class_id, field_index))
                .copied()
                .unwrap_or(cid)
        };
        let force_non_null = self
            .graph
            .static_final_non_null
            .contains(&(class_id, field_index));
        record_const_ref_meta(
            &mut self.graph,
            slot_addr,
            exact_cid,
            Some((class_id, field_index)),
            force_non_null,
        );
        if let Some(&(tag, len)) = self
            .const_folds
            .static_final_prim_arrays
            .get(&(class_id, field_index))
        {
            if let Some(entry) = self.graph.const_ref_meta.get_mut(&slot_addr) {
                entry.non_null = true;
                entry.prim_array_tag.get_or_insert(tag);
                entry.array_length.get_or_insert(len);
            }
        }
        Some(v)
    }

    /// Register resolved non-volatile `static final` primitive `(class_id, field_index) -> (type_tag, bits)`.
    pub fn add_static_final_prims_from_sites<I>(&mut self, iter: I)
    where
        I: IntoIterator<Item = ((u32, u32), (u8, i64))>,
    {
        self.const_folds.static_final_prims.extend(iter);
    }

    /// Emit `Op::Const` / `Op::ConstF` for an initialized non-volatile `static final`
    /// primitive field in `static_final_prims`.
    pub(in crate::ir) fn try_emit_static_final_prim(
        &mut self,
        class_id: u32,
        field_index: u32,
        type_tag: u8,
    ) -> Option<NodeId> {
        let &(saved_tag, bits) = self
            .const_folds
            .static_final_prims
            .get(&(class_id, field_index))?;
        if saved_tag != type_tag {
            return None;
        }
        let (op, ty) = match type_tag {
            b'Z' => (Op::Const(i64::from(bits != 0)), IrType::Int),
            b'B' => (Op::Const(i64::from(bits as i8)), IrType::Int),
            b'S' => (Op::Const(i64::from(bits as i16)), IrType::Int),
            b'C' => (Op::Const(i64::from(bits as u16)), IrType::Int),
            b'I' => (Op::Const(i64::from(bits as i32)), IrType::Int),
            b'J' => (Op::Const(bits), IrType::Long),
            b'F' => (Op::ConstF(u64::from(bits as u32)), IrType::Float),
            b'D' => (Op::ConstF(bits as u64), IrType::Double),
            _ => return None,
        };
        Some(self.graph.add(op, ty, vec![], None))
    }

    /// Emit a folded `static final` constant for reference (`IndyRootRef`) or primitive (`Const`/`ConstF`) fields.
    pub(in crate::ir) fn try_emit_static_final_fold(
        &mut self,
        class_id: u32,
        field_index: u32,
        type_tag: u8,
    ) -> Option<NodeId> {
        if matches!(type_tag, b'L' | b'[') {
            self.try_emit_static_final_root_ref(class_id, field_index)
        } else {
            self.try_emit_static_final_prim(class_id, field_index, type_tag)
        }
    }

    /// Record exact-class / non-null metadata for a folded constant field ref slot.
    pub(in crate::ir) fn record_folded_root_ref_meta(
        &mut self,
        slot_addr: usize,
        exact_class_id: u32,
    ) {
        record_const_ref_meta(&mut self.graph, slot_addr, exact_class_id, None, false);
    }

    /// `C.isAssignableFrom(o.getClass())` with `C` a constant mirror
    /// (`ConstFoldTables::const_mirror_classes`) is `o instanceof C`: `o` is
    /// non-null past its `getClass` call, and `instanceof` answers the same
    /// subtype question, interfaces and lambda proxies included, without the
    /// native call (`DirectMethodHandleAccessor.checkReceiver`).
    pub(in crate::ir) fn try_fold_class_is_assignable_from_get_class(
        &mut self,
        pc: usize,
        info_ptr: usize,
    ) -> bool {
        if info_ptr == 0 || self.stack.len() < 2 {
            return false;
        }
        // SAFETY: `info_ptr` is a planner-boxed `JitInvokeInfo` the artifact keeps alive.
        let info: &crate::JitInvokeInfo = unsafe { &*(info_ptr as *const crate::JitInvokeInfo) };
        if !(matches!(info.invoke_kind, 0 | 1)
            && info.class_name == "java/lang/Class"
            && info.method_name == "isAssignableFrom"
            && info.descriptor == "(Ljava/lang/Class;)Z")
        {
            return false;
        }
        let n = self.stack.len();
        let (Some(target), Some(source)) = (
            crate::ir::node_id_opt(self.stack[n - 2]),
            crate::ir::node_id_opt(self.stack[n - 1]),
        ) else {
            return false;
        };
        let Some(slot) = self.const_ref_slot_of(target) else {
            return false;
        };
        let Some((class_id, name)) = self.const_folds.const_mirror_classes.get(&slot).cloned()
        else {
            return false;
        };
        let Some(obj) = self.get_class_receiver(source) else {
            return false;
        };
        let Some(ctrl) = self.ctrl_opt() else {
            return false;
        };
        self.pop();
        self.pop();
        let (name_ptr, name_len) = crate::intern_typecheck_target(&name, Some(class_id));
        let v = self.graph.add(
            Op::InstanceOf {
                name_ptr: name_ptr as usize,
                name_len,
                seen_cell: 0,
                header_only: false,
            },
            IrType::Int,
            vec![ctrl, self.mem, obj],
            Some(pc),
        );
        self.mem = v;
        self.push(v);
        true
    }

    /// `(box class id, boxed primitive)` when `node` is an `Integer.valueOf(I)`
    /// / `Long.valueOf(J)` call: its answer is exactly that box class, and its
    /// final `value` field holds exactly the primitive it was given.
    fn box_call_of(&self, node: NodeId) -> Option<(u32, &'static str, u8, NodeId)> {
        let n = self.graph.nodes.get(node as usize)?;
        let Op::Call { info_ptr } = n.op else {
            return None;
        };
        if info_ptr == 0 || n.inputs.len() != 3 {
            return None;
        }
        // SAFETY: a planner-boxed `JitInvokeInfo` the artifact keeps alive.
        let info: &crate::JitInvokeInfo = unsafe { &*(info_ptr as *const crate::JitInvokeInfo) };
        let tag = match (
            info.invoke_kind,
            info.class_name,
            info.method_name,
            info.descriptor,
        ) {
            (3, "java/lang/Integer", "valueOf", "(I)Ljava/lang/Integer;") => b'I',
            (3, "java/lang/Long", "valueOf", "(J)Ljava/lang/Long;") => b'J',
            _ => return None,
        };
        (info.declaring_class_id != 0)
            .then(|| (info.declaring_class_id, info.class_name, tag, n.inputs[2]))
    }

    /// `getfield value` at `pc` (tag `type_tag`) of a box call: the boxed
    /// primitive itself (a spliced `intValue()` of `Integer.valueOf(x)` is `x`).
    pub(in crate::ir) fn box_call_payload(
        &self,
        base: NodeId,
        pc: usize,
        type_tag: u8,
    ) -> Option<NodeId> {
        let (box_class, _, tag, value) = self.box_call_of(base)?;
        (tag == type_tag && self.field_decl_class_by_pc.get(&pc) == Some(&box_class))
            .then_some(value)
    }

    /// Can a `checkcast` to the interned name `(name_ptr, name_len)` of a box
    /// call never fail? Its class is exactly the box class, which passes a cast
    /// to itself, `Number` and `Object`.
    pub(in crate::ir) fn box_call_passes_checkcast(
        &self,
        obj: NodeId,
        name_ptr: usize,
        name_len: usize,
    ) -> bool {
        let Some((_, box_name, _, _)) = self.box_call_of(obj) else {
            return false;
        };
        if name_ptr == 0 {
            return false;
        }
        // SAFETY: an interned type-check name, alive for the process.
        let name = unsafe { std::slice::from_raw_parts(name_ptr as *const u8, name_len) };
        name == box_name.as_bytes() || name == b"java/lang/Number" || name == b"java/lang/Object"
    }

    /// The object whose `Object.getClass()` call `node` is, or `None`.
    fn get_class_receiver(&self, node: NodeId) -> Option<NodeId> {
        let n = self.graph.nodes.get(node as usize)?;
        let Op::Call { info_ptr } = n.op else {
            return None;
        };
        if info_ptr == 0 {
            return None;
        }
        // SAFETY: as above.
        let info: &crate::JitInvokeInfo = unsafe { &*(info_ptr as *const crate::JitInvokeInfo) };
        if info.invoke_kind != 0
            || !crate::is_object_get_class_site(info.method_name, info.descriptor)
        {
            return None;
        }
        n.inputs.get(2).copied()
    }

    /// Emit end-of-build memo and throw-edge census lines when reporting is on.
    pub(in crate::ir) fn emit_build_censuses(&mut self) {
        // Round nio21 wave 1 (lane L2): the local throw edges' line.
        if !self.local_throw_handlers.is_empty() && crate::ir::ir_bail_reporting() {
            eprintln!(
                "[ir] local throw edges in {}: {} athrow(s) branch to a handler of this body, \
                 {} spliced athrow(s) by a type test \
                 ({} candidate handler(s), {} handler pc(s) walkable); {} of {} planned call \
                 site(s) have exception edges; {} of {} planned implicit site(s) do",
                self.method_label.as_deref().unwrap_or("<method>"),
                self.local_throw_edges,
                self.spliced_throw_edges,
                self.local_throw_handlers.len(),
                self.local_throw_optional.len(),
                self.call_throw_edges,
                self.call_throw_planned.len(),
                self.implicit_throw_edges,
                self.implicit_throw_planned.len(),
            );
        }
        if self.splices_done > 0 && crate::ir::ir_bail_reporting() {
            eprintln!(
                "[ir] spliced {} callee bod{} into {}",
                self.splices_done,
                if self.splices_done == 1 { "y" } else { "ies" },
                self.method_label.as_deref().unwrap_or("<method>"),
            );
        }
        // JIT round 23 wave 3b: the G14-1 census on the graph, for tests.
        {
            let c = &self.field_load_memo_census;
            self.graph.field_memo_census = (
                c.recorded,
                c.hits,
                c.forwarded,
                c.cleared_at_join,
                c.cleared_by_opaque,
            );
        }
        // Round 16 wave 14 (lane osrir, MON3-2): the static-load memo's line.
        if (self.static_load_memo_census.recorded > 0
            || self.static_load_memo_census.store_recorded > 0)
            && crate::ir::ir_bail_reporting()
        {
            let c = &self.static_load_memo_census;
            eprintln!(
                "[ir] static-load memo ({}) in {}: recorded={} hits={} forwarded={} \
                 cleared: join={} effect={} store={} \
                 stores: recorded={} ref={} hits={} forwarded={} spliced-forwarded={}",
                if self.static_load_memo_mode == crate::ir::StaticLoadMemoMode::Forward {
                    "forward"
                } else {
                    "census"
                },
                self.method_label.as_deref().unwrap_or("<method>"),
                c.recorded,
                c.hits,
                c.forwarded,
                c.cleared_at_join,
                c.cleared_by_effect,
                c.dropped_by_store,
                c.store_recorded,
                c.store_recorded_ref,
                c.store_hits,
                c.store_forwarded,
                c.splice_forwarded,
            );
        }
        // Round 15 wave 1 (lane chain): the G14-1 census line.
        if (self.field_load_memo_census.recorded > 0
            || self.field_load_memo_census.store_recorded > 0)
            && crate::ir::ir_bail_reporting()
        {
            let c = &self.field_load_memo_census;
            eprintln!(
                "[ir] field-load memo ({}) in {}: recorded={} hits={} across_splice={} \
                 forwarded={} cleared: join={} opaque={} ordered={} store={} \
                 field-identity={} ({}) stores: recorded={} hits={} forwarded={} ({}) \
                 final: hits={} forwarded={} ({})",
                if self.field_load_memo_mode == crate::ir::FieldLoadMemoMode::Forward {
                    "forward"
                } else {
                    "census"
                },
                self.method_label.as_deref().unwrap_or("<method>"),
                c.recorded,
                c.hits,
                c.hits_across_splice,
                c.forwarded,
                c.cleared_at_join,
                c.cleared_by_opaque,
                c.cleared_by_ordering,
                c.dropped_by_store,
                c.separated_by_field_identity,
                if self.graph.field_identity_alias {
                    "kept"
                } else {
                    "dropped"
                },
                c.store_recorded,
                c.store_hits,
                c.store_forwarded,
                if self.field_store_forward_on {
                    "forward"
                } else {
                    "census"
                },
                c.final_hits,
                c.final_forwarded,
                if self.field_final_forward_on {
                    "forward"
                } else {
                    "census"
                },
            );
        }
    }
}
