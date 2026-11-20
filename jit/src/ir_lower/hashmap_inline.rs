use super::*;

impl<'a> Lowerer<'a> {
    /// Emits `CMP DWORD PTR [base + disp32], reg32` (`39 /r`).
    fn emit_cmp_mem32_disp32_reg32(&mut self, base: u8, disp: i32, reg: u8) {
        let rex = 0x40u8 | (((reg >= 8) as u8) << 2) | ((base >= 8) as u8);
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit(&[0x39, 0x80 | ((reg & 7) << 3) | (base & 7)]);
        if base & 7 == 4 {
            self.buf.emit_byte(0x24);
        }
        self.buf.emit(&disp.to_le_bytes());
    }

    /// Emits the inline object/compact/class-id check for a plain non-array
    /// compact object in `reg`, jumping to `decline` on null, non-object,
    /// legacy layout, or mismatched `class_id`. Clobbers flags only.
    fn emit_compact_plain_exact_check(
        &mut self,
        reg: u8,
        class_id: u32,
        check_null: bool,
        exact_proven: bool,
        compact_proven: bool,
        decline: &mut Vec<usize>,
    ) {
        if check_null && !exact_proven {
            if reg >= 8 {
                self.buf
                    .emit(&[0x4D, 0x85, 0xC0 | ((reg & 7) << 3) | (reg & 7)]);
            } else {
                self.buf
                    .emit(&[0x48, 0x85, 0xC0 | ((reg & 7) << 3) | (reg & 7)]);
            }
            decline.push(self.emit_jcc_rel32(0x84)); // JZ
        }
        // Arrays and heap fillers never carry `GC_FLAG_COMPACT` (`types/src/heap_types.rs`),
        // so checking `GC_FLAG_COMPACT` when `!compact_proven` rejects them together.
        if !compact_proven {
            self.emit_str_test_mem8(
                reg,
                cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8, // Cast: asserted <= 127
                cratonvm_types::GC_FLAG_COMPACT,
            );
            decline.push(self.emit_jcc_rel32(0x84)); // JZ: legacy / array / non-compact
        } else if !exact_proven {
            self.emit_str_test_mem8(
                reg,
                cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: asserted <= 127
                cratonvm_types::KIND_TAG_BYTE_MASK,
            );
            decline.push(self.emit_jcc_rel32(0x85)); // JNE: array
        }
        if !exact_proven {
            self.emit_str_cmp_class_id(reg, class_id);
            decline.push(self.emit_jcc_rel32(0x85)); // JNE: other class
        }
    }

    /// Applies the ZGC colored-ref barrier to the 64-bit reference word in `RAX`
    /// when `zgc_base` is `Some(base)`. Clobbers `RAX`, `R11`, flags; preserves
    /// `RCX`, `RDX`, `R8`, `R9`, `R10`.
    fn emit_decode_rax_if_zgc(&mut self, zgc_base: Option<u64>, decline: &mut Vec<usize>) {
        if let Some(base) = zgc_base {
            let good = self.emit_colored_ref_word_barrier(base, decline);
            for p in good {
                self.patch_rel32_to_here(p);
            }
        }
    }

    /// Emits a read-only inline x86-64 prefix for `java/util/HashMap.get(Object)`
    /// / `java/util/Map.get(Object)` (`Integer` key fast path on exact compact
    /// `HashMap` / `HashMap$Node` instances), declining cleanly to the ordinary
    /// `Op::Call` on any unexpected receiver, key, layout, collision, `TreeNode`,
    /// or bad ZGC color.
    ///
    /// Scratch contract at `Op::Call` prefix: `RAX`, `RCX`, `RDX`, `R8`..`R11`
    /// and flags.
    pub(super) fn try_emit_ir_hashmap_get_prefix(&mut self, id: NodeId, info: &JitInvokeInfo) {
        if !crate::ir::ir_hashmap_get_inline_enabled() || cratonvm_types::narrow_oops_enabled() {
            return;
        }
        if !(info.class_name == "java/util/HashMap" || info.class_name == "java/util/Map")
            || info.method_name != "get"
            || info.descriptor != "(Ljava/lang/Object;)Ljava/lang/Object;"
            || !matches!(info.invoke_kind, 0 | 1 | 2)
        {
            return;
        }
        let zgc_base = if crate::x64::zgc_read_barrier_blocks_inline_fields() {
            let Some(base) = crate::x64::zgc_colored_inline_field_load_base() else {
                return;
            };
            Some(base)
        } else {
            None
        };
        let Some(layout) = crate::ir_collection_layouts_current().and_then(|l| l.hash_map) else {
            return;
        };
        const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
        const _: () = assert!(cratonvm_types::GC_FLAGS_BYTE_OFFSET <= 127);
        const _: () = assert!(
            cratonvm_types::ARRAY_LENGTH_OFFSET == 8 && cratonvm_types::ARRAY_DATA_OFFSET == 16
        );

        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        if node.inputs.len() != 4 || node.ty != IrType::Ref {
            return;
        }
        let (recv, key) = (node.inputs[2], node.inputs[3]);
        if self.carry_names(recv) || self.carry_names(key) {
            return;
        }
        let Some(slot) = self.prefix_result_slot(id, IrType::Ref) else {
            return;
        };

        const R8: u8 = 8;
        const R9: u8 = 9;
        let mut decline: Vec<usize> = Vec::new();
        self.note_rcx_written();
        if !self.unbox_layout_epoch_proven() {
            if let Some(p) = self.emit_layout_epoch_guard() {
                decline.push(p);
            }
        }
        if self.zgc_fused_gate.is_some() && zgc_base.is_none() {
            self.emit_zgc_unarmed_check(&mut decline);
        }

        // Load RAX = map, R10 = key; verify exact compact HashMap and Integer.
        self.gp_load_value(RAX, recv);
        self.gp_load_value(R10, key);
        let recv_exact = self.unbox_exact_class_proven(recv, layout.hashmap_class_id)
            || self.checkcast_exact_proven(recv, layout.hashmap_class_id);
        let recv_compact = self.unbox_compact_proven(recv, layout.hashmap_class_id);
        let recv_null = !recv_exact && !self.receiver_already_non_null(recv);
        self.emit_compact_plain_exact_check(
            RAX,
            layout.hashmap_class_id,
            recv_null,
            recv_exact,
            recv_compact,
            &mut decline,
        );
        let key_exact = self.unbox_exact_class_proven(key, layout.integer_class_id)
            || self.checkcast_exact_proven(key, layout.integer_class_id);
        let key_compact = self.unbox_compact_proven(key, layout.integer_class_id);
        let key_null = !key_exact && !self.receiver_already_non_null(key);
        self.emit_compact_plain_exact_check(
            R10,
            layout.integer_class_id,
            key_null,
            key_exact,
            key_compact,
            &mut decline,
        );

        // Read RAX = map.table (and barrier if ZGC), while R8D = key.value, EDX = h = v ^ (v >>> 16).
        self.emit_load_base_disp32(RAX, RAX, layout.table_compact_offset, true);
        self.emit_decode_rax_if_zgc(zgc_base, &mut decline);
        self.emit_str_test_r64(RAX);
        decline.push(self.emit_jcc_rel32(0x84)); // JZ: empty/null table

        self.emit_load_base_disp32(R8, R10, layout.integer_value_compact_offset, false);
        self.buf.emit(&[0x44, 0x89, 0xC2]); // MOV EDX, R8D
        self.buf.emit(&[0x89, 0xD1]); // MOV ECX, EDX
        self.buf.emit(&[0xC1, 0xE9, 0x10]); // SHR ECX, 16
        self.buf.emit(&[0x31, 0xCA]); // XOR EDX, ECX  (EDX = HashMap.hash(key))

        // R11D = table.length; require len > 0; idx = (len - 1) & h < len.
        self.emit_load_base_disp32(R11, RAX, 8, false);
        self.buf.emit(&[0x45, 0x85, 0xDB]); // TEST R11D, R11D
        decline.push(self.emit_jcc_rel32(0x8E)); // JLE: empty table
        self.buf.emit(&[0x41, 0xFF, 0xCB]); // DEC R11D
        self.buf.emit(&[0x41, 0x21, 0xD3]); // AND R11D, EDX

        // RAX = table[idx] (clobbers R11 in ZGC decode; R8D, R10, EDX stay intact).
        self.buf.emit(&[0x4A, 0x8B, 0x44, 0xD8, 0x10]); // MOV RAX, QWORD [RAX + R11*8 + 16]
        self.emit_decode_rax_if_zgc(zgc_base, &mut decline);
        self.emit_str_test_r64(RAX);
        let bucket_empty = self.emit_jcc_rel32(0x84); // JZ -> return null

        // Verify compact `HashMap$Node` and matching `node.hash == h`.
        self.emit_compact_plain_exact_check(
            RAX,
            layout.node_class_id,
            false,
            false,
            false,
            &mut decline,
        );
        self.emit_cmp_mem32_disp32_reg32(RAX, layout.node_hash_compact_offset, RDX);
        decline.push(self.emit_jcc_rel32(0x85)); // JNE: hash collision / chain

        // Save R9 = node, load RAX = node.key (decoded under ZGC).
        self.buf.emit(&[0x49, 0x89, 0xC1]); // MOV R9, RAX
        self.emit_load_base_disp32(RAX, R9, layout.node_key_compact_offset, true);
        self.emit_decode_rax_if_zgc(zgc_base, &mut decline);
        self.buf.emit(&[0x4C, 0x39, 0xD0]); // CMP RAX, R10 (nk == key)
        let key_ptr_hit = self.emit_jcc_rel32(0x84); // JE -> load node.value

        // Check `nk` is an Integer with `nk.value == R8D`.
        self.emit_compact_plain_exact_check(
            RAX,
            layout.integer_class_id,
            true,
            false,
            false,
            &mut decline,
        );
        self.emit_cmp_mem32_disp32_reg32(RAX, layout.integer_value_compact_offset, R8);
        decline.push(self.emit_jcc_rel32(0x85)); // JNE: chain/miss

        // Hit: load `RAX = node.value` (decoded under ZGC) and store straight through.
        self.patch_rel32_to_here(key_ptr_hit);
        self.emit_load_base_disp32(RAX, R9, layout.node_value_compact_offset, true);
        self.emit_decode_rax_if_zgc(zgc_base, &mut decline);
        let store_res = self.buf.pos();
        self.emit_prefix_result_store(id, slot);
        let hit = self.emit_jmp_rel32();

        // Empty bucket: return `null` (out of line so hit falls straight through).
        self.patch_rel32_to_here(bucket_empty);
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
        let empty_store = self.emit_jmp_rel32();
        self.patch_rel32_to_target(empty_store, store_res);

        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }
}
