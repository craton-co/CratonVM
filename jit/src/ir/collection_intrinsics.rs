use super::*;

impl IrBuilder {
    /// Expands `java/util/ArrayList` and `java/util/List` `size()` / `get(int)`
    /// into explicit guarded IR nodes (`Guard`, `ExactClassIs`, `Op::Load`,
    /// `Op::ArrayLength`, `Op::ArrayLoad(MemKind::Ref)`) when `self.splice` is
    /// empty. Works across all collectors (Serial, G1, ZGC) and both execution
    /// modes, exposing loop-invariant `ArrayList` metadata to LICM and masked
    /// bounds hoisting.
    pub(in crate::ir) fn try_collection_access_intrinsic(
        &mut self,
        pc: usize,
        info: &JitInvokeInfo,
        num_args: usize,
    ) -> bool {
        let is_al_or_list =
            info.class_name == "java/util/ArrayList" || info.class_name == "java/util/List";
        if !is_al_or_list || !matches!(info.invoke_kind, 0 | 1 | 2) {
            return false;
        }
        let is_size = info.method_name == "size" && info.descriptor == "()I" && num_args == 1;
        let is_get = info.method_name == "get"
            && info.descriptor == "(I)Ljava/lang/Object;"
            && num_args == 2;
        if !is_size && !is_get {
            return false;
        }
        if !self.has_frame_state_at(pc) || self.string_access_withdrawn_pcs.contains(&pc) {
            return false;
        }
        let Some(layout) = crate::ir_collection_layouts_current().and_then(|l| l.array_list) else {
            return false;
        };
        if layout.class_id == 0 {
            return false;
        }

        let ctrl = self.ctrl;
        let idx = if is_get { Some(self.pop()) } else { None };
        let recv = self.pop();

        // 1. Guard receiver non-null and exact `java/util/ArrayList` class.
        let null_ref = self.graph.add(Op::Const(0), IrType::Ref, vec![], None);
        let recv_ne = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![recv, null_ref], pc);
        let mut license_guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, recv_ne],
            Some(pc),
        );
        if self.static_final_exact_class_of(recv) != Some(layout.class_id) {
            let exact = self.add_data(
                Op::ExactClassIs {
                    class_id: layout.class_id,
                },
                IrType::Int,
                vec![recv],
                pc,
            );
            license_guard = self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, exact],
                Some(pc),
            );
        }

        // 2. Load `ArrayList.size` (`int`).
        let size_idx_node = self.iconst(i64::from(layout.size_field_index));
        let size = self.graph.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            with_licensing_guard(vec![ctrl, self.mem, recv, size_idx_node], license_guard),
            Some(pc),
        );
        self.mem = size;
        if layout.size_compact_offset != u32::MAX && cratonvm_types::compact_ref_fields_enabled() {
            self.graph
                .field_rows
                .insert(size, (layout.size_compact_offset, false, b'I'));
        }
        self.graph.synthetic_field_access_pcs.insert(pc);

        if is_size {
            self.push(size);
            return true;
        }

        // 3. Load `ArrayList.elementData` (`Object[]`), guard non-null and length.
        let ed_idx_node = self.iconst(i64::from(layout.element_data_field_index));
        let element_data = self.graph.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            with_licensing_guard(vec![ctrl, self.mem, recv, ed_idx_node], license_guard),
            Some(pc),
        );
        self.mem = element_data;
        if layout.element_data_compact_offset != u32::MAX
            && cratonvm_types::compact_ref_fields_enabled()
        {
            self.graph.field_rows.insert(
                element_data,
                (layout.element_data_compact_offset, true, b'L'),
            );
        }

        let ed_ne = self.add_data(
            Op::Cmp(CmpOp::Ne),
            IrType::Int,
            vec![element_data, null_ref],
            pc,
        );
        let ed_guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, ed_ne],
            Some(pc),
        );
        let arr_len = self.graph.add(
            Op::ArrayLength,
            IrType::Int,
            with_licensing_guard(vec![ctrl, self.mem, element_data], ed_guard),
            Some(pc),
        );
        self.mem = arr_len;

        let idx = idx.expect("is_get checked above");
        let zero = self.iconst(0);
        let ge_zero = self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![idx, zero], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, ge_zero],
            Some(pc),
        );
        let lt_size = self.add_data(Op::Cmp(CmpOp::Lt), IrType::Int, vec![idx, size], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, lt_size],
            Some(pc),
        );
        let lt_len = self.add_data(Op::Cmp(CmpOp::Lt), IrType::Int, vec![idx, arr_len], pc);
        let bounds_guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, lt_len],
            Some(pc),
        );

        let elem = self.graph.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            with_guard_token(vec![ctrl, self.mem, element_data, idx], bounds_guard),
            Some(pc),
        );
        self.mem = elem;
        self.push(elem);
        true
    }

    /// Attempts pre-splice expansion of collection / StringBuilder intrinsics
    /// at `pc` using `self.invoke_info`.
    pub(in crate::ir) fn try_pre_splice_collection_at(&mut self, pc: usize) -> bool {
        self.invoke_info
            .get(&pc)
            .copied()
            .is_some_and(|(info_ptr, num_args, _)| {
                self.try_pre_splice_collection_intrinsic(pc, info_ptr, num_args)
            })
    }

    /// Tries `ArrayList`/`List`, `HashMap`/`Map.get`, `StringBuilder`, and
    /// `String.equals` IR intrinsics ahead of splice admission when `self.splice` is empty.
    pub(in crate::ir) fn try_pre_splice_collection_intrinsic(
        &mut self,
        pc: usize,
        info_ptr: usize,
        num_args: usize,
    ) -> bool {
        if !self.splice.is_empty() || info_ptr == 0 {
            return false;
        }
        // SAFETY: `info_ptr` is non-zero and addresses a `JitInvokeInfo` boxed
        // onto the compiled artifact for the life of the compile.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if super::ir_arraylist_ir_intrinsics_enabled()
            && self.try_collection_access_intrinsic(pc, info, num_args)
        {
            return true;
        }
        if self.try_hashmap_get_intrinsic(pc, info, num_args) {
            return true;
        }
        if self.try_string_builder_ir_intrinsic(pc, info, num_args) {
            return true;
        }
        self.try_string_equals_intrinsic(pc, info_ptr, info, num_args)
    }

    /// Fast inline reference-equality diamond for `String.equals(Object)Z`:
    /// `Guard(recv != null)`, `If(recv == arg)` -> `1`, else `Op::Call`.
    fn try_string_equals_intrinsic(
        &mut self,
        pc: usize,
        info_ptr: usize,
        info: &JitInvokeInfo,
        num_args: usize,
    ) -> bool {
        if info.class_name != "java/lang/String"
            || info.method_name != "equals"
            || info.descriptor != "(Ljava/lang/Object;)Z"
            || num_args != 2
            || !matches!(info.invoke_kind, 0 | 1)
        {
            return false;
        }
        if !self.has_frame_state_at(pc)
            || self.string_access_withdrawn_pcs.contains(&pc)
            || self.ctrl_opt().is_none()
            || self.stack.len() < 2
            || !self.monitors.is_empty()
            || self.sync_direct_calls.contains_key(&pc)
            || self.call_throw_planned.contains_key(&pc)
            || self.catch_arm_calls.contains_key(&pc)
        {
            return false;
        }
        let at = self.stack.len() - 2;
        if self.graph.node_opt(self.stack[at]).map(|n| n.ty) != Some(IrType::Ref)
            || self.graph.node_opt(self.stack[at + 1]).map(|n| n.ty) != Some(IrType::Ref)
        {
            return false;
        }
        let ctrl0 = self.ctrl;
        let mem0 = self.mem;
        let arg = self.pop();
        let recv = self.pop();
        let null_ref = self.graph.add(Op::Const(0), IrType::Ref, vec![], None);
        let recv_ne = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![recv, null_ref], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl0, recv_ne],
            Some(pc),
        );
        let eq = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![recv, arg], pc);
        let iff = self
            .graph
            .add(Op::If, IrType::Control, vec![ctrl0, eq], Some(pc));
        let true_ctrl = self
            .graph
            .add(Op::Proj(0), IrType::Control, vec![iff], Some(pc));
        let false_ctrl = self
            .graph
            .add(Op::Proj(1), IrType::Control, vec![iff], Some(pc));
        let call = self.graph.add(
            Op::Call { info_ptr },
            IrType::Int,
            vec![false_ctrl, mem0, recv, arg],
            Some(pc),
        );
        let join = self.graph.add(
            Op::Merge,
            IrType::Control,
            vec![true_ctrl, false_ctrl],
            Some(pc),
        );
        self.ctrl = join;
        self.mem = self
            .graph
            .add(Op::Phi, IrType::Memory, vec![join, mem0, call], Some(pc));
        let one = self.iconst(1);
        let res = self
            .graph
            .add(Op::Phi, IrType::Int, vec![join, one, call], Some(pc));
        self.push(res);
        crate::ir_evidence::note_builder_expansion();
        true
    }

    fn guard_receiver_exact(
        &mut self,
        ctrl: NodeId,
        val: NodeId,
        class_id: u32,
        pc: usize,
    ) -> NodeId {
        if self.exact_receiver_node_proof(val, class_id) {
            return NO_NODE;
        }
        // `ExactClassIs` already returns `0` on null and deopts at `pc`, so no
        // separate preceding `Guard(val != null)` is needed.
        let exact = self.add_data(Op::ExactClassIs { class_id }, IrType::Int, vec![val], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, exact],
            Some(pc),
        )
    }

    /// True when `key` is loaded from a live frozen `static final` array whose
    /// every element is non-null with exact class `class_id`, recording CHA
    /// invalidation dependencies at `pc`.
    fn key_from_frozen_exact_array(
        &mut self,
        ctrl: NodeId,
        key: NodeId,
        class_id: u32,
        pc: usize,
    ) -> bool {
        let Some(arr) = self
            .graph
            .node_opt(key)
            .filter(|n| n.op == Op::ArrayLoad(MemKind::Ref))
            .and_then(|n| n.input_opt(2))
        else {
            return false;
        };
        let Some(facts) = self.frozen_facts.as_ref().map(std::sync::Arc::clone) else {
            return false;
        };
        let (arr_id, known, root_key) = if let Some(id) = self.frozen_object_of(arr) {
            (id, true, None)
        } else {
            let Some(&Op::IndyRootRef { slot_addr }) = self.graph.node_opt(arr).map(|n| &n.op)
            else {
                return false;
            };
            let Some(rk) = self
                .graph
                .const_ref_meta
                .get(&slot_addr)
                .and_then(|m| m.static_field)
            else {
                return false;
            };
            let Some(&id) = facts.roots.get(&rk) else {
                return false;
            };
            let same_slot = facts
                .objects
                .get(id as usize)
                .is_some_and(|o| o.slot_addr != 0 && o.slot_addr == slot_addr);
            (id, same_slot, Some(rk))
        };
        let Some(obj) = facts.live(arr_id) else {
            return false;
        };
        if obj.elements.is_empty()
            || obj.array_len.is_none_or(|len| len as usize != obj.elements.len())
            || !obj.elements.iter().all(|elem| {
                matches!(
                    elem,
                    Some(crate::frozen_facts::FrozenValue::Ref(eid))
                        if facts.objects.get(*eid as usize).is_some_and(|eo| eo.class_id == class_id)
                )
            })
        {
            return false;
        }
        if !known {
            if obj.slot_addr == 0 {
                return false;
            }
            let want = self.graph.add(
                Op::IndyRootRef {
                    slot_addr: obj.slot_addr,
                },
                IrType::Ref,
                vec![ctrl],
                Some(pc),
            );
            let same = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![arr, want], pc);
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, same],
                Some(pc),
            );
            self.graph.frozen_identity_guards += 1;
        }
        for owner in [
            root_key
                .and_then(|rk| facts.root_owners.get(&rk))
                .map(|s| s.as_ref()),
            obj.elem_cha_owner.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        {
            let dep = format!("{}{owner}", crate::frozen_facts::CHA_FIELD_DEP_PREFIX);
            if !self.graph.frozen_cha_deps.contains(&dep) {
                self.graph.frozen_cha_deps.push(dep);
            }
        }
        true
    }

    fn insert_compact_field_row(
        &mut self,
        node: NodeId,
        compact_offset: i32,
        is_ref: bool,
        tag: u8,
    ) {
        if cratonvm_types::compact_ref_fields_enabled() {
            if let Ok(off) = u32::try_from(compact_offset) {
                self.graph.field_rows.insert(node, (off, is_ref, tag));
            }
        }
    }

    /// Expands `HashMap.get(Object)` / `Map.get(Object)` for exact `HashMap` +
    /// exact `Integer` key single-node hit into guarded IR nodes.
    pub(in crate::ir) fn try_hashmap_get_intrinsic(
        &mut self,
        pc: usize,
        info: &JitInvokeInfo,
        num_args: usize,
    ) -> bool {
        let is_hm_or_map =
            info.class_name == "java/util/HashMap" || info.class_name == "java/util/Map";
        if !is_hm_or_map
            || info.method_name != "get"
            || info.descriptor != "(Ljava/lang/Object;)Ljava/lang/Object;"
            || num_args != 2
            || !matches!(info.invoke_kind, 0 | 1 | 2)
        {
            return false;
        }
        if !super::ir_hashmap_get_inline_enabled()
            || cratonvm_types::narrow_oops_enabled()
            || self.string_access_withdrawn_pcs.contains(&pc)
            || !self.has_frame_state_at(pc)
        {
            return false;
        }
        let Some(layout) = crate::ir_collection_layouts_current().and_then(|l| l.hash_map) else {
            return false;
        };
        if layout.hashmap_class_id == 0
            || layout.node_class_id == 0
            || layout.integer_class_id == 0
            || layout.table_field_index == u32::MAX
            || layout.node_hash_field_index == u32::MAX
            || layout.node_key_field_index == u32::MAX
            || layout.node_value_field_index == u32::MAX
            || layout.integer_value_field_index == u32::MAX
        {
            return false;
        }

        let ctrl = self.ctrl;
        let key = self.pop();
        let recv = self.pop();
        let null_ref = self.graph.add(Op::Const(0), IrType::Ref, vec![], None);
        let zero = self.iconst(0);
        let one = self.iconst(1);

        let recv_license = self.guard_receiver_exact(ctrl, recv, layout.hashmap_class_id, pc);
        let table_idx = self.iconst(i64::from(layout.table_field_index));
        let table = self.graph.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            with_licensing_guard(vec![ctrl, self.mem, recv, table_idx], recv_license),
            Some(pc),
        );
        self.mem = table;
        self.insert_compact_field_row(table, layout.table_compact_offset, true, b'L');
        self.graph.synthetic_field_access_pcs.insert(pc);

        let table_ne = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![table, null_ref], pc);
        let table_guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, table_ne],
            Some(pc),
        );
        let len = self.graph.add(
            Op::ArrayLength,
            IrType::Int,
            with_licensing_guard(vec![ctrl, self.mem, table], table_guard),
            Some(pc),
        );
        self.mem = len;
        let mask = self.add_data(Op::Sub, IrType::Int, vec![len, one], pc);
        let mask_ge_zero = self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![mask, zero], pc);
        let mask_ge_zero_guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, mask_ge_zero],
            Some(pc),
        );
        let len_and_mask = self.add_data(Op::And, IrType::Int, vec![len, mask], pc);
        let pow2 = self.add_data(
            Op::Cmp(CmpOp::Eq),
            IrType::Int,
            vec![len_and_mask, zero],
            pc,
        );
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, pow2],
            Some(pc),
        );

        let key_license = if self.exact_receiver_node_proof(key, layout.integer_class_id)
            || self.key_from_frozen_exact_array(ctrl, key, layout.integer_class_id, pc)
        {
            NO_NODE
        } else {
            self.guard_receiver_exact(ctrl, key, layout.integer_class_id, pc)
        };
        let kv_idx = self.iconst(i64::from(layout.integer_value_field_index));
        let kv = self.graph.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            with_licensing_guard(vec![ctrl, self.mem, key, kv_idx], key_license),
            Some(pc),
        );
        self.mem = kv;
        self.insert_compact_field_row(kv, layout.integer_value_compact_offset, false, b'I');

        let sixteen = self.iconst(16);
        let shifted = self.add_data(Op::UShr, IrType::Int, vec![kv, sixteen], pc);
        let h = self.add_data(Op::Xor, IrType::Int, vec![kv, shifted], pc);
        let idx = self.add_data(Op::And, IrType::Int, vec![mask, h], pc);

        let node = self.graph.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            with_guard_token(vec![ctrl, self.mem, table, idx], mask_ge_zero_guard),
            Some(pc),
        );
        self.mem = node;

        let node_license = self.guard_receiver_exact(ctrl, node, layout.node_class_id, pc);
        let nh_idx = self.iconst(i64::from(layout.node_hash_field_index));
        let nh = self.graph.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            with_licensing_guard(vec![ctrl, self.mem, node, nh_idx], node_license),
            Some(pc),
        );
        self.mem = nh;
        self.insert_compact_field_row(nh, layout.node_hash_compact_offset, false, b'I');
        let nh_eq = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![nh, h], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, nh_eq],
            Some(pc),
        );

        let nk_idx = self.iconst(i64::from(layout.node_key_field_index));
        let nk = self.graph.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            with_licensing_guard(vec![ctrl, self.mem, node, nk_idx], node_license),
            Some(pc),
        );
        self.mem = nk;
        self.insert_compact_field_row(nk, layout.node_key_compact_offset, true, b'L');
        let nk_eq = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![nk, key], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, nk_eq],
            Some(pc),
        );

        let val_idx = self.iconst(i64::from(layout.node_value_field_index));
        let val = self.graph.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            with_licensing_guard(vec![ctrl, self.mem, node, val_idx], node_license),
            Some(pc),
        );
        self.mem = val;
        self.insert_compact_field_row(val, layout.node_value_compact_offset, true, b'L');
        self.push(val);
        true
    }

    /// Expands `java/lang/StringBuilder` `length()I`, `append(C)`, and
    /// `setLength(I)V` into guarded IR loads and stores when `sb_chain_active`
    /// is idle.
    pub(in crate::ir) fn try_string_builder_ir_intrinsic(
        &mut self,
        pc: usize,
        info: &JitInvokeInfo,
        num_args: usize,
    ) -> bool {
        if info.class_name != "java/lang/StringBuilder" || !matches!(info.invoke_kind, 0 | 1 | 2) {
            return false;
        }
        let kind = match (info.method_name, info.descriptor, num_args) {
            ("length", "()I", 1) => 0u8,
            ("append", "(C)Ljava/lang/StringBuilder;", 2) => 1u8,
            ("setLength", "(I)V", 2) => 2u8,
            _ => return false,
        };
        if !crate::ir_sb_prefix_enabled()
            || cratonvm_types::narrow_oops_enabled()
            || self.string_access_withdrawn_pcs.contains(&pc)
            || !self.has_frame_state_at(pc)
            || self.sb_chain_active.is_some()
        {
            return false;
        }
        let Some(b) = self.string_layout.and_then(|l| l.builder) else {
            return false;
        };
        if b.class_id == 0
            || b.count_field_index == u32::MAX
            || b.value_field_index == u32::MAX
            || b.coder_field_index == u32::MAX
        {
            return false;
        }

        let ctrl = self.ctrl;
        let arg = if kind != 0 { Some(self.pop()) } else { None };
        let recv = self.pop();
        let license = self.guard_receiver_exact(ctrl, recv, b.class_id, pc);
        self.graph.synthetic_field_access_pcs.insert(pc);

        let count_idx = self.iconst(i64::from(b.count_field_index));
        if kind == 0 {
            let cnt = self.graph.add(
                Op::Load(MemKind::Int),
                IrType::Int,
                with_licensing_guard(vec![ctrl, self.mem, recv, count_idx], license),
                Some(pc),
            );
            self.mem = cnt;
            self.insert_compact_field_row(cnt, b.count_compact_offset, false, b'I');
            self.push(cnt);
            return true;
        }

        let zero = self.iconst(0);
        if kind == 2 {
            let new_len = arg.expect("setLength arg");
            let ge0 = self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![new_len, zero], pc);
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, ge0],
                Some(pc),
            );
            let cnt = self.graph.add(
                Op::Load(MemKind::Int),
                IrType::Int,
                with_licensing_guard(vec![ctrl, self.mem, recv, count_idx], license),
                Some(pc),
            );
            self.mem = cnt;
            self.insert_compact_field_row(cnt, b.count_compact_offset, false, b'I');
            let le_cnt = self.add_data(Op::Cmp(CmpOp::Le), IrType::Int, vec![new_len, cnt], pc);
            self.graph.add(
                Op::Guard { bci: pc },
                IrType::Void,
                vec![ctrl, le_cnt],
                Some(pc),
            );
            let st = self.graph.add(
                Op::Store(MemKind::Int),
                IrType::Memory,
                vec![ctrl, self.mem, recv, count_idx, new_len],
                Some(pc),
            );
            self.insert_compact_field_row(st, b.count_compact_offset, false, b'I');
            self.mem = st;
            return true;
        }

        // append(C)Ljava/lang/StringBuilder;
        let ch = arg.expect("append(C) arg");
        let eight = self.iconst(8);
        let hi = self.add_data(Op::UShr, IrType::Int, vec![ch, eight], pc);
        let hi_eq0 = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![hi, zero], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, hi_eq0],
            Some(pc),
        );

        let (coder_kind, coder_tag) = if b.coder_compact_is_byte {
            (MemKind::Byte, b'B')
        } else {
            (MemKind::Int, b'I')
        };
        let coder_idx = self.iconst(i64::from(b.coder_field_index));
        let coder = self.graph.add(
            Op::Load(coder_kind),
            IrType::Int,
            with_licensing_guard(vec![ctrl, self.mem, recv, coder_idx], license),
            Some(pc),
        );
        self.mem = coder;
        self.insert_compact_field_row(coder, b.coder_compact_offset, false, coder_tag);
        let coder_eq0 = self.add_data(Op::Cmp(CmpOp::Eq), IrType::Int, vec![coder, zero], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, coder_eq0],
            Some(pc),
        );

        let value_idx = self.iconst(i64::from(b.value_field_index));
        let val_arr = self.graph.add(
            Op::Load(MemKind::Ref),
            IrType::Ref,
            with_licensing_guard(vec![ctrl, self.mem, recv, value_idx], license),
            Some(pc),
        );
        self.mem = val_arr;
        self.insert_compact_field_row(val_arr, b.value_compact_offset, true, b'L');
        let null_ref = self.graph.add(Op::Const(0), IrType::Ref, vec![], None);
        let val_ne = self.add_data(Op::Cmp(CmpOp::Ne), IrType::Int, vec![val_arr, null_ref], pc);
        let val_guard = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, val_ne],
            Some(pc),
        );

        let cnt = self.graph.add(
            Op::Load(MemKind::Int),
            IrType::Int,
            with_licensing_guard(vec![ctrl, self.mem, recv, count_idx], license),
            Some(pc),
        );
        self.mem = cnt;
        self.insert_compact_field_row(cnt, b.count_compact_offset, false, b'I');

        let cap = self.graph.add(
            Op::ArrayLength,
            IrType::Int,
            with_licensing_guard(vec![ctrl, self.mem, val_arr], val_guard),
            Some(pc),
        );
        self.mem = cap;

        let cnt_ge0 = self.add_data(Op::Cmp(CmpOp::Ge), IrType::Int, vec![cnt, zero], pc);
        self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, cnt_ge0],
            Some(pc),
        );
        let cnt_lt_cap = self.add_data(Op::Cmp(CmpOp::Lt), IrType::Int, vec![cnt, cap], pc);
        let _bounds_g = self.graph.add(
            Op::Guard { bci: pc },
            IrType::Void,
            vec![ctrl, cnt_lt_cap],
            Some(pc),
        );

        let arr_st = self.graph.add(
            Op::ArrayStore(MemKind::Byte),
            IrType::Memory,
            vec![ctrl, self.mem, val_arr, cnt, ch],
            Some(pc),
        );
        self.mem = arr_st;
        let one = self.iconst(1);
        let next_cnt = self.add_data(Op::Add, IrType::Int, vec![cnt, one], pc);
        let cnt_st = self.graph.add(
            Op::Store(MemKind::Int),
            IrType::Memory,
            vec![ctrl, self.mem, recv, count_idx, next_cnt],
            Some(pc),
        );
        self.insert_compact_field_row(cnt_st, b.count_compact_offset, false, b'I');
        self.mem = cnt_st;
        self.push(recv);
        true
    }
}

#[cfg(test)]
#[path = "collection_intrinsics_tests.rs"]
mod tests;
